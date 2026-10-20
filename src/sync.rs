//! 从服务器拉取会话日志并导入 CC Switch。

use crate::{cc_switch_bin, root};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

pub struct Outcome {
    pub host: String,
    pub imported: i64,
    pub scanned: i64,
    pub mirror_files: usize,
    pub err: Option<String>,
}

/// 同步单台服务器：拉取 Codex / Claude 会话日志 → 调 cc-switch 导入用量。
pub fn sync_host(host: &str, since: i64, quiet: bool) -> Outcome {
    let dest = root().join(host);
    if let Err(e) = fs::create_dir_all(dest.join("codex").join("sessions")) {
        return fail(host, format!("创建镜像目录失败: {e}"));
    }
    if let Err(e) = fs::create_dir_all(dest.join("claude").join("projects")) {
        return fail(host, format!("创建镜像目录失败: {e}"));
    }

    for (remote_sub, local_sub) in [
        (".codex/sessions", "codex/sessions"),
        (".claude/projects", "claude/projects"),
    ] {
        let target = dest.join(local_sub);
        if let Err(e) = pull(host, remote_sub, since, &target) {
            let detail = crate::hosts::probe(host)
                .err
                .unwrap_or_else(|| format!("{e:#}"));
            return fail(host, detail);
        }
        if !quiet {
            // 同步过程中不打印逐文件细节，保持输出干净
        }
    }

    let (imported, scanned) = match import(host) {
        Ok(v) => v,
        Err(e) => return fail(host, format!("cc-switch 导入失败: {e:#}")),
    };

    Outcome {
        host: host.to_string(),
        imported,
        scanned,
        mirror_files: count_jsonl(&dest),
        err: None,
    }
}

fn fail(host: &str, msg: String) -> Outcome {
    Outcome {
        host: host.to_string(),
        imported: 0,
        scanned: 0,
        mirror_files: 0,
        err: Some(msg),
    }
}

/// tar-over-SSH 增量拉取（服务器上通常没装 rsync，tar 到处都有）。
fn pull(host: &str, remote_sub: &str, since: i64, local: &Path) -> Result<()> {
    let minutes = ((chrono::Local::now().timestamp() - since).max(60) / 60) + 5;
    let remote = format!(
        r#"d="$HOME/{sub}"; [ -d "$d" ] || {{ printf '' | tar -czf - -T - 2>/dev/null; exit 0; }}
cd "$d" || exit 0
if find . -maxdepth 0 -newermt '@0' >/dev/null 2>&1; then
  find . -type f -name '*.jsonl' -newermt '@{since}' 2>/dev/null
else
  find . -type f -name '*.jsonl' -mmin -{minutes} 2>/dev/null
fi | tar -czf - -T - 2>/dev/null
"#,
        sub = remote_sub,
        since = since,
        minutes = minutes,
    );

    let mut ssh = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "ServerAliveInterval=20",
            "-o",
            "ServerAliveCountMax=3",
        ])
        .arg(host)
        .arg(&remote)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("无法启动 ssh（主机 {host}）"))?;

    let stdout = ssh
        .stdout
        .take()
        .context("无法获取 ssh stdout")?;

    let tar = Command::new("tar")
        .arg("xzf")
        .arg("-")
        .arg("-C")
        .arg(local)
        .stdin(Stdio::from(stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("无法启动本地 tar")?;

    let tar_out = tar.wait_with_output()?;
    let ssh_status = ssh.wait()?;

    if !ssh_status.success() {
        bail!("ssh 退出码 {:?}", ssh_status.code());
    }
    if !tar_out.status.success() {
        let msg = String::from_utf8_lossy(&tar_out.stderr)
            .lines()
            .last()
            .unwrap_or("tar 解包失败")
            .to_string();
        bail!("{msg}");
    }
    Ok(())
}

/// 调用 `cc-switch sessions sync-usage`，用作用域环境变量只扫该主机的镜像目录。
fn import(host: &str) -> Result<(i64, i64)> {
    let base = root().join(host);
    let out = Command::new(cc_switch_bin())
        .args(["sessions", "sync-usage", "--all", "--json"])
        .env("CODEX_HOME", base.join("codex"))
        .env("CLAUDE_CONFIG_DIR", base.join("claude"))
        .output()
        .context("跑不起来 cc-switch，检查是否安装或 `ccr config` 里配置路径")?;

    if !out.status.success() {
        bail!(
            "cc-switch 退出码 {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")
        );
    }

    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .context("cc-switch 输出不是预期 JSON（版本不匹配？）")?;
    Ok((
        v["imported"].as_i64().unwrap_or(0),
        v["filesScanned"].as_i64().unwrap_or(0),
    ))
}

/// 递归统计目录下的 .jsonl 文件数。
pub fn count_jsonl(dir: &Path) -> usize {
    fn walk(d: &Path, n: &mut usize) {
        let Ok(rd) = fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, n);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n
}
