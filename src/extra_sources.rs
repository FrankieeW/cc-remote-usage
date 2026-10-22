use crate::{hosts, state, sync};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

pub fn validate_root(root: &str) -> Result<()> {
    if !root.starts_with('/')
        || root.trim_matches('/').is_empty()
        || root.chars().any(char::is_control)
        || root.split('/').any(|part| part == "..")
    {
        bail!("额外 Codex 目录须为远端绝对路径，不能是 / 或包含 ..、控制字符：{root:?}");
    }
    Ok(())
}

pub fn validate_config(cfg: &state::Config) -> Result<()> {
    for (host, roots) in &cfg.extra_codex_roots {
        if !hosts::valid_host_id(host) {
            bail!("额外 Codex 目录的主机名不合法：{host}");
        }
        for root in roots {
            validate_root(root)?;
        }
    }
    Ok(())
}

pub fn pull(host: &str, since: i64, roots: &[String]) -> Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    let base = state::root().join(host);
    let snapshot = serde_json::to_vec(roots)?;
    let already_imported =
        fs::read(base.join(".codex-roots.json")).is_ok_and(|previous| previous == snapshot);
    let since = if already_imported { since } else { 0 };
    let staging = tempfile::Builder::new()
        .prefix(".extra-codex-")
        .tempdir_in(&base)
        .context("创建额外日志暂存目录失败")?;
    let script = remote_script(roots, since, state::now_epoch());
    sync::pull_archive(host, &script, staging.path())?;
    flatten(
        staging.path(),
        staging.path(),
        &base.join("codex/sessions/extra"),
    )
}

pub fn record_imported(host: &str, roots: &[String]) -> Result<()> {
    state::write_atomic(
        &state::root().join(host).join(".codex-roots.json"),
        &serde_json::to_vec(roots)?,
    )
}

fn flatten(dir: &Path, staging: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            flatten(&path, staging, destination)?;
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            let parent = path.parent().context("会话路径没有父目录")?;
            let relative = parent.strip_prefix(staging)?;
            let hash = Sha256::digest(relative.as_os_str().as_encoded_bytes());
            let bucket = destination.join(format!("{hash:x}"));
            fs::create_dir_all(&bucket)?;
            fs::rename(&path, bucket.join(entry.file_name()))
                .with_context(|| format!("保存额外会话 {} 失败", path.display()))?;
        }
    }
    Ok(())
}

fn remote_script(roots: &[String], since: i64, now: i64) -> String {
    let since = (since - sync::OVERLAP_SECS).max(0);
    let minutes = (now - since).max(60) / 60 + 1;
    let quoted = roots
        .iter()
        .map(|root| format!("'{}'", root.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"empty() {{ printf '' | tar -czf - -T - 2>/dev/null; }}
list=$(mktemp "${{TMPDIR:-/tmp}}/ccr-sources.XXXXXX") || {{ empty; exit 1; }}
trap 'rm -f "$list"' 0
trap 'exit 1' 1 2 15
for d in {quoted}; do
  [ -d "$d" ] || {{ empty; printf '额外 Codex 目录不存在：%s\n' "$d" >&2; exit 1; }}
  d=$(cd "$d" && pwd -P) || {{ empty; exit 1; }}
  if find "$d" -maxdepth 0 -newermt '@0' >/dev/null 2>&1; then
    find "$d" \( -type d \( -name materials -o -name .git -o -name .venv -o -name node_modules \) ! -path "$d" -prune \) -o \( -type f -name '*.jsonl' \( -path '*/sessions/*' -o -path '*/archived_sessions/*' \) -newermt '@{since}' -print \) >> "$list"
  else
    find "$d" \( -type d \( -name materials -o -name .git -o -name .venv -o -name node_modules \) ! -path "$d" -prune \) -o \( -type f -name '*.jsonl' \( -path '*/sessions/*' -o -path '*/archived_sessions/*' \) -mmin -{minutes} -print \) >> "$list"
  fi
  rc=$?
  [ "$rc" -eq 0 ] || {{ empty; exit "$rc"; }}
done
tar -czf - -T "$list"
rc=$?
if [ "$rc" -eq 1 ] && tar --version 2>/dev/null | grep -q 'GNU tar'; then exit 0; fi
exit "$rc"
"#
    )
}
