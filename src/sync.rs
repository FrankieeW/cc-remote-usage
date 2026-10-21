//! 从服务器拉取会话日志并导入 CC Switch。

use crate::ssh;
use crate::state::{cc_switch_bin, root};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

pub struct Outcome {
    pub host: String,
    pub imported: i64,
    pub scanned: i64,
    pub mirror_files: usize,
    /// cc-switch 导入时报告的非致命错误（个别文件解析失败等），同步本身仍算成功
    pub warnings: Vec<String>,
    pub err: Option<String>,
}

/// 远端目录 → 本机镜像子目录
const PULLS: [(&str, &str); 2] = [
    (".codex/sessions", "codex/sessions"),
    (".claude/projects", "claude/projects"),
];

/// CC Switch 5.x 的 `--all` 会连带扫描本机 Gemini / OpenCode / Hermes / OpenClaw / Pi 的日志
/// （它们的目录不受 CODEX_HOME / CLAUDE_CONFIG_DIR 控制），导入数里会混进本机用量。
/// 所以按 app 逐个导入，只扫作用域化后的镜像目录。
const IMPORT_APPS: [&str; 2] = ["codex", "claude"];

/// 增量窗口往前多看 5 分钟，抵消服务器与本机的时钟偏差；
/// 重复拉到的内容由 CC Switch 按字节偏移去重。
const OVERLAP_SECS: i64 = 300;

/// 同步单台服务器：拉取 Codex / Claude 会话日志 → 调 cc-switch 导入用量。
pub fn sync_host(host: &str, since: i64) -> Outcome {
    let dest = root().join(host);
    for (remote_sub, local_sub) in PULLS {
        let target = dest.join(local_sub);
        if let Err(e) = fs::create_dir_all(&target) {
            return fail(host, format!("创建镜像目录失败: {e}"));
        }
        if let Err(e) = pull(host, remote_sub, since, &target) {
            return fail(host, format!("{e:#}"));
        }
    }

    match import(&dest) {
        Ok(stats) => Outcome {
            host: host.to_string(),
            imported: stats.imported,
            scanned: stats.scanned,
            mirror_files: count_jsonl(&dest),
            warnings: stats.warnings,
            err: None,
        },
        Err(e) => fail(host, format!("cc-switch 导入失败: {e:#}")),
    }
}

fn fail(host: &str, msg: String) -> Outcome {
    Outcome {
        host: host.to_string(),
        imported: 0,
        scanned: 0,
        mirror_files: 0,
        warnings: Vec::new(),
        err: Some(msg),
    }
}

/// 远端脚本：把 since 之后改过的 .jsonl 打成 tar.gz 写到 stdout。
/// - 目录不存在 / 没有新文件时同样输出合法的空归档（GNU tar 与 bsdtar 都支持），本地解包不报错；
/// - BSD find 不认 `-newermt @epoch` 时退回 `-mmin`；
/// - GNU tar 在文件读到一半被 agent 追加时以 1 退出（"file changed as we read it"），
///   归档本身完整可用（读到的是一致的前缀，下轮会整份重拉），所以 GNU tar 的 1 算成功；
///   bsdtar 的 1 却表示有文件读不了被跳过，必须算失败，否则断点推进、那个文件就漏了；
///   tar 的 stderr 不屏蔽，失败时能看到原因。
fn remote_pull_script(remote_sub: &str, since: i64, now: i64) -> String {
    let since = (since - OVERLAP_SECS).max(0);
    let minutes = (now - since).max(60) / 60 + 1;
    format!(
        r#"empty() {{ printf '' | tar -czf - -T - 2>/dev/null; }}
d="$HOME/{remote_sub}"
[ -d "$d" ] && cd "$d" 2>/dev/null || {{ empty; exit 0; }}
if find . -maxdepth 0 -newermt '@0' >/dev/null 2>&1; then
  find . -type f -name '*.jsonl' -newermt '@{since}' 2>/dev/null
else
  find . -type f -name '*.jsonl' -mmin -{minutes} 2>/dev/null
fi | tar -czf - -T -
rc=$?
if [ "$rc" -eq 1 ] && tar --version 2>/dev/null | grep -q 'GNU tar'; then exit 0; fi
exit "$rc"
"#
    )
}

/// 单个目录的传输上限：连接还活着但远端命令卡死（如挂死的网络盘）时，
/// ServerAlive 检测不到，没有上限就会一直占着运行锁、后面的同步全被跳过。
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// cc-switch 导入的上限（首次导入大量日志也就几分钟；卡住多半是数据库被锁）
const IMPORT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// tar-over-SSH 增量拉取（服务器上通常没装 rsync，tar 到处都有）。
fn pull(host: &str, remote_sub: &str, since: i64, local: &Path) -> Result<()> {
    let remote = remote_pull_script(remote_sub, since, chrono::Local::now().timestamp());
    let mut ssh = ssh::spawn_script(host, &ssh::TRANSFER, &remote, Stdio::piped())
        .with_context(|| format!("无法启动 ssh（主机 {host}）"))?;

    let stdout = ssh.stdout.take().context("无法获取 ssh stdout")?;
    let stderr = ssh.stderr.take().context("无法获取 ssh stderr")?;
    // stderr 单独线程读：不读的话 ssh 写满管道就会卡住，tar 也跟着等
    let stderr_reader = thread::spawn(move || ssh::read_tail(stderr, 64 * 1024));
    let watchdog = ssh::Watchdog::arm(ssh.id(), TRANSFER_TIMEOUT);

    let tar_out = Command::new("tar")
        .arg("xzf")
        .arg("-")
        .arg("-C")
        .arg(local)
        .stdin(Stdio::from(stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|tar| tar.wait_with_output());
    let tar_out = match tar_out {
        Ok(o) => o,
        Err(e) => {
            let _ = ssh.kill();
            let _ = ssh.wait();
            watchdog.disarm();
            return Err(e).context("本地 tar 运行失败");
        }
    };
    let ssh_status = ssh.wait();
    let timed_out = watchdog.disarm();
    let ssh_err = stderr_reader.join().unwrap_or_default();
    let ssh_status = ssh_status.context("等待 ssh 退出失败")?;
    // 计时恰好在 ssh 自己退出之后才到点时不算超时（那次 kill 落空了）
    if timed_out && ssh_status.code().is_none() {
        bail!(
            "传输超过 {} 分钟仍未完成，已中止（下轮重试）",
            TRANSFER_TIMEOUT.as_secs() / 60
        );
    }

    let tar_err = (!tar_out.status.success()).then(|| {
        ssh::last_line(&String::from_utf8_lossy(&tar_out.stderr))
            .unwrap_or_else(|| "tar 解包失败".to_string())
    });
    if !ssh_status.success() {
        let reason = ssh::describe_exit(ssh_status.code(), &ssh_err);
        match tar_err {
            Some(t) => bail!("{reason}；本地 tar: {t}"),
            None => bail!("{reason}"),
        }
    }
    if let Some(t) = tar_err {
        bail!("本地 tar 解包失败: {t}");
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
struct ImportStats {
    imported: i64,
    scanned: i64,
    /// cc-switch 报告的文件级错误、暂缓导入等，不影响本次同步算成功
    warnings: Vec<String>,
}

/// 调用 `cc-switch sessions sync-usage --app <app>`，用作用域环境变量只扫该主机的镜像目录。
fn import(base: &Path) -> Result<ImportStats> {
    let bin = cc_switch_bin();
    let mut total = ImportStats::default();
    for app in IMPORT_APPS {
        let child = Command::new(&bin)
            .args(["sessions", "sync-usage", "--app", app, "--json"])
            .env("CODEX_HOME", base.join("codex"))
            .env("CLAUDE_CONFIG_DIR", base.join("claude"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("跑不起来 cc-switch，检查是否安装或 `ccr config` 里配置路径")?;
        let watchdog = ssh::Watchdog::arm(child.id(), IMPORT_TIMEOUT);
        let out = child.wait_with_output();
        let timed_out = watchdog.disarm();
        let out = out.context("等待 cc-switch 退出失败")?;
        if timed_out && out.status.code().is_none() {
            bail!(
                "cc-switch（--app {app}）超过 {} 分钟没有返回，已中止",
                IMPORT_TIMEOUT.as_secs() / 60
            );
        }

        if !out.status.success() {
            bail!(
                "cc-switch（--app {app}）退出码 {:?}: {}",
                out.status.code(),
                ssh::last_line(&String::from_utf8_lossy(&out.stderr)).unwrap_or_default()
            );
        }
        let stats = parse_import(&out.stdout).with_context(|| format!("cc-switch --app {app}"))?;
        total.imported += stats.imported;
        total.scanned += stats.scanned;
        total.warnings.extend(stats.warnings);
    }
    Ok(total)
}

/// 解析 `sync-usage --json`（CC Switch 5.x：imported / filesScanned / deferredFiles / errors …）。
/// 缺 `imported` 视为上游改了格式，明确报错，而不是悄悄记成 0。
fn parse_import(stdout: &[u8]) -> Result<ImportStats> {
    let v: serde_json::Value =
        serde_json::from_slice(stdout).context("cc-switch 输出不是预期 JSON（版本不匹配？）")?;
    let Some(imported) = v["imported"].as_i64() else {
        bail!("cc-switch 输出缺少 imported 字段（版本不匹配？）");
    };
    let mut warnings: Vec<String> = v["errors"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|e| {
                    e.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| e.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    let deferred = v["deferredFiles"].as_i64().unwrap_or(0);
    if deferred > 0 {
        // 用量还没入库，排最前面，免得被文件级错误挤出显示范围
        warnings.insert(
            0,
            format!("{deferred} 个文件被暂缓导入（多为子代理会话的父会话不在镜像里），下轮会重试"),
        );
    }
    Ok(ImportStats {
        imported,
        scanned: v["filesScanned"].as_i64().unwrap_or(0),
        warnings,
    })
}

/// 导入提示的显示：最多列 3 条，其余只给条数。
pub fn warning_summary(warnings: &[String]) -> Vec<String> {
    const SHOWN: usize = 3;
    let mut lines: Vec<String> = warnings.iter().take(SHOWN).cloned().collect();
    if warnings.len() > SHOWN {
        lines.push(format!("…另有 {} 条", warnings.len() - SHOWN));
    }
    lines
}

/// 递归统计目录下的 .jsonl 文件数（不跟随符号链接）。
pub fn count_jsonl(dir: &Path) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => count_jsonl(&e.path()),
            Ok(t) if t.is_file() && e.path().extension().is_some_and(|x| x == "jsonl") => 1,
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::{scratch, touch};
    use std::io::Write;

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn parse_import_reads_cc_switch_5_json() {
        let out = br#"{"imported":12,"skipped":0,"filesScanned":41,"suspectedDuplicates":0,
            "deferredFiles":0,"errors":["bad.jsonl: unexpected EOF",{"file":"x","message":"y"}]}"#;
        let s = parse_import(out).unwrap();
        assert_eq!((s.imported, s.scanned), (12, 41));
        assert_eq!(s.warnings[0], "bad.jsonl: unexpected EOF");
        assert_eq!(s.warnings.len(), 2, "对象形式的错误也要保留");
    }

    #[test]
    fn parse_import_reports_deferred_files() {
        // CC Switch 5.x：子代理会话（fork）的父会话不在扫描目录里时会暂缓导入
        let s = parse_import(
            br#"{"imported":0,"filesScanned":3,"deferredFiles":2,"errors":["a.jsonl: bad line"]}"#,
        )
        .unwrap();
        assert_eq!(s.warnings.len(), 2);
        assert!(
            s.warnings[0].contains("2 个文件被暂缓导入"),
            "暂缓导入意味着用量还没入库，排在最前面，不能被别的提示挤掉: {:?}",
            s.warnings
        );
    }

    #[test]
    fn warning_summary_shows_a_few_and_counts_the_rest() {
        let many: Vec<String> = (1..=5).map(|i| format!("w{i}")).collect();
        assert_eq!(warning_summary(&many), ["w1", "w2", "w3", "…另有 2 条"]);
        assert_eq!(warning_summary(&many[..2]), ["w1", "w2"]);
        assert!(warning_summary(&[]).is_empty());
    }

    #[test]
    fn parse_import_rejects_unexpected_shapes() {
        assert!(parse_import(b"not json").is_err());
        let renamed = parse_import(br#"{"importedCount":3,"filesScanned":1}"#).unwrap_err();
        assert!(
            format!("{renamed:#}").contains("imported"),
            "字段改名必须报错，不能当成 0"
        );
    }

    /// 本机直接跑远端脚本（macOS 上即 BSD find + bsdtar），返回 (退出码, 归档内容)。
    /// `fake_bin` 放在 PATH 最前面，用来模拟其他平台上 find / tar 的行为。
    fn run_pull_script(
        home: &Path,
        remote_sub: &str,
        since: i64,
        now: i64,
        fake_bin: Option<&Path>,
    ) -> (Option<i32>, Vec<u8>) {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(remote_pull_script(remote_sub, since, now))
            .env("HOME", home);
        if let Some(bin) = fake_bin {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{path}", bin.display()));
        }
        let out = cmd.output().unwrap();
        (out.status.code(), out.stdout)
    }

    /// 校验是 gzip 归档（空列表也必须是），返回里面的文件名。
    fn list_archive(archive: &[u8]) -> Vec<String> {
        assert!(
            archive.starts_with(&[0x1f, 0x8b]),
            "输出必须是 gzip 归档（bsdtar 对空输入也返回 0，只看退出码测不出来）"
        );
        let mut tar = Command::new("tar")
            .args(["tzf", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        tar.stdin.take().unwrap().write_all(archive).unwrap();
        let listing = tar.wait_with_output().unwrap();
        assert!(
            listing.status.success(),
            "归档必须合法: {}",
            String::from_utf8_lossy(&listing.stderr)
        );
        let mut names: Vec<String> = String::from_utf8_lossy(&listing.stdout)
            .lines()
            .map(|l| l.trim_start_matches("./").to_string())
            .filter(|l| !l.is_empty())
            .collect();
        names.sort();
        names
    }

    fn archive_listing(home: &Path, remote_sub: &str, since: i64, now: i64) -> Vec<String> {
        let (code, archive) = run_pull_script(home, remote_sub, since, now, None);
        assert_eq!(code, Some(0));
        list_archive(&archive)
    }

    /// 在临时 bin 目录放一个同名包装脚本，转调真正的工具。
    fn fake_tool(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn pull_script_falls_back_to_mmin_without_newermt() {
        let home = scratch("pull-mmin");
        let bin = scratch("pull-mmin-bin");
        let real = crate::state::which("find").unwrap();
        // 模拟不认 -newermt 的 find（老 BSD / BusyBox）
        fake_tool(
            &bin,
            "find",
            &format!(
                r#"for a in "$@"; do [ "$a" = -newermt ] && exit 1; done; exec "{real}" "$@""#
            ),
        );
        let now = chrono::Local::now().timestamp();
        touch(&home.join(".codex/sessions/new.jsonl"), HOUR / 2);
        touch(&home.join(".codex/sessions/old.jsonl"), HOUR * 24 * 20);

        let (code, archive) =
            run_pull_script(&home, ".codex/sessions", now - 3600, now, Some(&bin));

        assert_eq!(code, Some(0));
        assert_eq!(list_archive(&archive), ["new.jsonl"]);
    }

    #[test]
    fn pull_script_tolerates_only_gnu_tars_file_changed_exit() {
        // GNU tar 以 1 退出只表示"读到一半文件被追加"，归档完整可用；
        // bsdtar 的 1 却是真错误（读不了的文件被跳过），不能当成功，否则断点推进、文件就漏了
        let home = scratch("pull-tar-rc");
        let now = chrono::Local::now().timestamp();
        touch(&home.join(".codex/sessions/live.jsonl"), HOUR / 2);
        let real = crate::state::which("tar").unwrap();
        let cases = [
            ("tar (GNU tar) 1.35", 1, 0),
            ("tar (GNU tar) 1.35", 2, 2),
            ("bsdtar 3.5.3 - libarchive 3.5.3", 1, 1),
        ];
        for (i, (version, tar_rc, script_rc)) in cases.into_iter().enumerate() {
            let bin = scratch(&format!("pull-tar-rc-bin-{i}"));
            fake_tool(
                &bin,
                "tar",
                &format!(
                    r#"[ "$1" = --version ] && {{ echo "{version}"; exit 0; }}
"{real}" "$@"; exit {tar_rc}"#
                ),
            );
            let (code, _) = run_pull_script(&home, ".codex/sessions", now - 3600, now, Some(&bin));
            assert_eq!(code, Some(script_rc), "{version} 退出码 {tar_rc}");
        }
    }

    #[test]
    fn pull_script_ships_recent_logs_with_overlap() {
        let home = scratch("pull-home");
        let now = chrono::Local::now().timestamp();
        touch(&home.join(".codex/sessions/2026/10/21/new.jsonl"), HOUR / 2);
        touch(
            &home.join(".codex/sessions/2026/10/21/edge.jsonl"),
            HOUR + HOUR / 30,
        );
        touch(
            &home.join(".codex/sessions/2026/10/01/old.jsonl"),
            HOUR * 24 * 20,
        );
        touch(&home.join(".codex/sessions/2026/10/21/notes.txt"), HOUR / 2);

        assert_eq!(
            archive_listing(&home, ".codex/sessions", now - 3600, now),
            ["2026/10/21/edge.jsonl", "2026/10/21/new.jsonl"],
            "since 之前 2 分钟改过的文件也要带上（时钟偏差余量），老文件和非 .jsonl 不带"
        );
    }

    #[test]
    fn pull_script_always_emits_a_valid_archive() {
        let home = scratch("pull-empty");
        let now = chrono::Local::now().timestamp();
        touch(&home.join(".codex/sessions/old.jsonl"), HOUR * 24 * 20);

        assert!(
            archive_listing(&home, ".claude/projects", 0, now).is_empty(),
            "目录不存在"
        );
        assert!(
            archive_listing(&home, ".codex/sessions", now - 60, now).is_empty(),
            "没有新文件"
        );
    }

    #[test]
    fn count_jsonl_ignores_other_files() {
        let d = scratch("count");
        touch(&d.join("a/b/1.jsonl"), HOUR);
        touch(&d.join("a/2.jsonl"), HOUR);
        touch(&d.join("a/3.txt"), HOUR);
        assert_eq!(count_jsonl(&d), 2);
    }
}
