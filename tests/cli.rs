//! 端到端：用假的 ssh / cc-switch 跑真正的 ccr 二进制，覆盖 sync 的完整流程
//! （锁、主机选择、退避、拉取、按 app 导入、落盘、清理）。
//!
//! 每个用例一套独立的临时目录，环境变量只设在子进程上，可以并行跑，也不会碰本机真实的
//! ~/.cc-switch。假 ssh 把收到的 `sh -s` 脚本放到 `$FAKE_REMOTES/<主机>` 当 HOME 在本机执行，
//! 所以远端脚本走的是真实的 find / tar（macOS 上即 BSD 工具）。

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

const DAY: Duration = Duration::from_secs(86_400);

/// 假 ssh：记录参数与调用类型（probe / pull），`dead*` 或没有远端目录的主机一律连不上；
/// `xxx-lan` 是 `xxx` 的别名（同一个 HOME）。
const FAKE_SSH: &str = r#"#!/bin/sh
echo "args $*" >> "$FAKE_LOG"
while [ $# -gt 0 ]; do
  case "$1" in
    -T) shift ;;
    -o) shift 2 ;;
    *) break ;;
  esac
done
host=$1; shift
case "$host" in *-lan) home=${host%-lan} ;; *) home=$host ;; esac
if [ ! -d "$FAKE_REMOTES/$home" ] || [ "${host#dead}" != "$host" ]; then
  echo "ssh: connect to host $host port 22: Operation timed out" >&2
  exit 255
fi
script=$(cat)
case "$script" in *__CCR__*) kind=probe ;; *) kind=pull ;; esac
echo "ssh $kind $host" >> "$FAKE_LOG"
case "$host" in
  stall*)
    # 模拟远端命令被杀：吐出半个 gzip 头就卡住，管道永远不关闭，
    # 本地 tar 等不到 EOF。ccr 必须靠"多久没有新字节"自己收场。
    printf '\037\213\010\000\000\000\000\000\000\003'
    sleep 20
    exit 0
    ;;
esac
printf '%s\n' "$script" | HOME="$FAKE_REMOTES/$home" "$@"
"#;

/// 假 cc-switch：记录作用域环境变量与参数，回一份 CC Switch 5.x 形状的 JSON。
const FAKE_CC_SWITCH: &str = r#"#!/bin/sh
if [ "$1" = --version ]; then
  printf 'cc-switch %s\n' "${FAKE_CC_VERSION:-5.11.3}"
  exit 0
fi
echo "cc-switch CODEX_HOME=$CODEX_HOME CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR $*" >> "$FAKE_LOG"
if [ "${FAKE_CC_FAIL_IMPORT:-}" = 1 ]; then
  echo 'database unavailable' >&2
  exit 1
fi
echo '{"imported":1,"skipped":0,"filesScanned":1,"suspectedDuplicates":0,"deferredFiles":0,"errors":[]}'
"#;

/// 假 cc-fleet：PATH 里先找到它，就不会退回去调本机装着的 CC Switch Fleet.app。
const FAKE_CC_FLEET: &str = r#"#!/bin/sh
echo '{"data":{"hosts":[]}}'
"#;

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("ccr-e2e-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for sub in ["root", "home/.ssh", "bin", "remotes"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let sb = Sandbox { dir };
        sb.executable("bin/ssh", FAKE_SSH);
        sb.executable("bin/cc-switch", FAKE_CC_SWITCH);
        sb.executable("bin/cc-fleet", FAKE_CC_FLEET);
        sb
    }

    fn executable(&self, rel: &str, body: &str) {
        let p = self.dir.join(rel);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn root(&self) -> PathBuf {
        self.dir.join("root")
    }

    fn remote(&self, host: &str) -> PathBuf {
        self.dir.join("remotes").join(host)
    }

    fn ssh_config(&self, text: &str) {
        fs::write(self.dir.join("home/.ssh/config"), text).unwrap();
    }

    fn ccr(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ccr"));
        cmd.args(args)
            .env("HOME", self.dir.join("home"))
            .env("REMOTE_USAGE_ROOT", self.root())
            .env("PATH", path)
            .env("FAKE_REMOTES", self.dir.join("remotes"))
            .env("FAKE_LOG", self.dir.join("calls.log"))
            .env_remove("REMOTE_USAGE_HOSTS");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.join("calls.log")).unwrap_or_default()
    }

    /// 跑 ccr 并给一个上限：超时就杀掉，防止测试自己挂死。
    /// 返回 (输出, 是否超时)。
    fn ccr_with_timeout(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        limit: Duration,
    ) -> (Output, bool) {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ccr"));
        cmd.args(args)
            .env("HOME", self.dir.join("home"))
            .env("REMOTE_USAGE_ROOT", self.root())
            .env("PATH", path)
            .env("FAKE_REMOTES", self.dir.join("remotes"))
            .env("FAKE_LOG", self.dir.join("calls.log"))
            .env_remove("REMOTE_USAGE_HOSTS")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let deadline = std::time::Instant::now() + limit;
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return (child.wait_with_output().unwrap(), true);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        (child.wait_with_output().unwrap(), false)
    }

    fn state(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.root().join("state.json")).unwrap()).unwrap()
    }
}

fn touch(p: &Path, age: Duration) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b"{}\n").unwrap();
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - age).unwrap();
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// 带一份近期 Codex + Claude 日志，外加一份 4 天前的 Codex 日志的远端。
/// 4 天：在首次回溯窗口（days = 2）之外、镜像保留期（retain_days = 7）之内——
/// 万一被错拉回来，清理也不会把它删掉，测试才能发现。
fn seed_remote(sb: &Sandbox, host: &str) {
    let r = sb.remote(host);
    touch(
        &r.join(".codex/sessions/2026/10/21/rollout-new.jsonl"),
        DAY / 24,
    );
    touch(&r.join(".claude/projects/p/s.jsonl"), DAY / 24);
    touch(
        &r.join(".codex/sessions/2026/10/17/rollout-older.jsonl"),
        DAY * 4,
    );
}

#[test]
fn sync_pulls_imports_per_app_prunes_and_records_failures() {
    let sb = Sandbox::new("sync");
    seed_remote(&sb, "alpha");
    let stale = sb
        .root()
        .join("alpha/codex/sessions/2026/01/02/rollout-stale.jsonl");
    touch(&stale, DAY * 30);

    let out = sb.ccr(&["sync", "alpha", "deadbox"], &[]);
    assert!(!out.status.success(), "部分同步失败必须返回非零退出码");

    let mirror = sb.root().join("alpha");
    assert!(mirror
        .join("codex/sessions/2026/10/21/rollout-new.jsonl")
        .exists());
    assert!(mirror.join("claude/projects/p/s.jsonl").exists());
    assert!(
        !mirror
            .join("codex/sessions/2026/10/17/rollout-older.jsonl")
            .exists(),
        "首次只回溯 days 天"
    );
    assert!(!stale.exists(), "导入后清理超过保留期的镜像");

    let calls = sb.calls();
    assert!(calls.contains("RemoteCommand=none") && calls.contains("BatchMode=yes"));
    let codex_home = format!("CODEX_HOME={}", mirror.join("codex").display());
    for app in ["--app codex", "--app claude"] {
        assert!(
            calls
                .lines()
                .any(|l| l.contains(&codex_home) && l.contains(app)),
            "缺少作用域化的 {app} 导入：\n{calls}"
        );
    }
    assert!(!calls.contains("--all"), "不能用 --all（会扫本机其他 app）");

    let state = sb.state();
    assert_eq!(state["hosts"]["alpha"]["failures"], 0);
    assert!(state["hosts"]["alpha"]["last_epoch"].as_i64().unwrap() > 0);
    assert_eq!(state["hosts"]["deadbox"]["failures"], 1);
    let err = state["hosts"]["deadbox"]["last_error"].as_str().unwrap();
    assert!(
        err.contains("Operation timed out"),
        "要带上 ssh 的原话: {err}"
    );
}

#[test]
fn second_sync_skips_while_the_lock_is_held() {
    let sb = Sandbox::new("lock");
    seed_remote(&sb, "alpha");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(sb.root().join("sync.lock"))
        .unwrap();
    lock.try_lock().unwrap();

    let out = sb.ccr(&["sync", "alpha"], &[]);

    assert!(out.status.success());
    assert!(
        text(&out).contains("另一个 ccr sync 正在运行"),
        "{}",
        text(&out)
    );
    assert!(!sb.calls().contains("ssh "), "持锁期间不能发起任何同步");
}

#[test]
fn daemon_host_list_still_respects_backoff() {
    let sb = Sandbox::new("backoff");
    seed_remote(&sb, "alpha");
    let until = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    fs::write(
        sb.root().join("state.json"),
        format!(r#"{{"hosts":{{"deadbox":{{"failures":3,"next_attempt":{until}}}}}}}"#),
    )
    .unwrap();

    let out = sb.ccr(
        &["sync", "--all", "--quiet"],
        &[("REMOTE_USAGE_HOSTS", "deadbox alpha")],
    );

    assert!(out.status.success(), "{}", text(&out));
    let calls = sb.calls();
    assert!(calls.contains("ssh pull alpha"));
    assert!(
        !calls.contains("deadbox"),
        "冷却中的主机不能再赔一次超时：\n{calls}"
    );
}

#[test]
fn hosts_from_config_are_validated() {
    let sb = Sandbox::new("config-hosts");
    fs::write(
        sb.root().join("config.json"),
        r#"{"hosts":["-oProxyCommand=touch /tmp/ccr-pwned"]}"#,
    )
    .unwrap();

    let out = sb.ccr(&["sync"], &[]);

    assert!(!out.status.success());
    assert!(text(&out).contains("不合法"), "{}", text(&out));
    assert!(sb.calls().is_empty(), "不合法的主机名不能传给 ssh");
}

#[test]
fn broken_config_never_triggers_pruning() {
    let sb = Sandbox::new("config-broken");
    seed_remote(&sb, "alpha");
    // 用户本意是不清理（retain_days: 0），但多了个逗号，配置解析失败
    fs::write(sb.root().join("config.json"), r#"{"retain_days": 0,}"#).unwrap();
    let stale = sb
        .root()
        .join("alpha/codex/sessions/2026/01/02/rollout-stale.jsonl");
    touch(&stale, DAY * 30);

    let out = sb.ccr(&["sync", "alpha"], &[]);

    assert!(!out.status.success(), "损坏的配置不能绕过版本锁定");
    assert!(text(&out).contains("解析失败"));
    assert!(stale.exists(), "配置读不出来时不能按默认值删文件");
    assert!(sb.calls().is_empty());
}

#[test]
fn pinned_importer_is_checked_before_discovering_hosts() {
    let sb = Sandbox::new("version-preflight");
    sb.ssh_config("Host deadbox\n  HostName 10.0.0.1\n");
    fs::write(
        sb.root().join("config.json"),
        r#"{"cc_switch_version":"5.11.3"}"#,
    )
    .unwrap();

    let out = sb.ccr(&["sync", "--all"], &[("FAKE_CC_VERSION", "5.12.0")]);

    assert!(!out.status.success());
    assert!(sb.calls().is_empty(), "版本不匹配时不能探测 SSH 或导入");
    assert!(!sb.root().join("state.json").exists());
}

#[test]
fn config_pins_installed_importer_and_can_unpin_it() {
    let sb = Sandbox::new("pin-config");

    let pinned = sb.ccr(&["config", "--pin-cc-switch"], &[]);

    assert!(pinned.status.success(), "{}", text(&pinned));
    let config: Value =
        serde_json::from_str(&fs::read_to_string(sb.root().join("config.json")).unwrap()).unwrap();
    assert_eq!(config["cc_switch_version"], "5.11.3");

    let unpinned = sb.ccr(&["config", "--unpin-cc-switch"], &[]);

    assert!(unpinned.status.success(), "{}", text(&unpinned));
    let config: Value =
        serde_json::from_str(&fs::read_to_string(sb.root().join("config.json")).unwrap()).unwrap();
    assert!(config["cc_switch_version"].is_null());
}

#[test]
fn local_import_retries_without_ssh_or_advancing_the_pull_checkpoint() {
    let sb = Sandbox::new("local-import");
    seed_remote(&sb, "alpha");
    let synced = sb.ccr(&["sync", "alpha"], &[]);
    assert!(synced.status.success(), "{}", text(&synced));
    let checkpoint = sb.state()["hosts"]["alpha"]["last_epoch"].clone();
    let previous_calls = sb.calls().len();

    let failed = sb.ccr(&["import", "alpha"], &[("FAKE_CC_FAIL_IMPORT", "1")]);

    assert!(!failed.status.success());
    assert_eq!(sb.state()["hosts"]["alpha"]["failures"], 1);
    assert_eq!(sb.state()["hosts"]["alpha"]["last_epoch"], checkpoint);

    let imported = sb.ccr(&["import", "alpha"], &[]);

    assert!(imported.status.success(), "{}", text(&imported));
    let state = sb.state();
    assert_eq!(state["hosts"]["alpha"]["last_epoch"], checkpoint);
    assert_eq!(state["hosts"]["alpha"]["failures"], 0);
    assert_eq!(state["hosts"]["alpha"]["next_attempt"], 0);
    assert_eq!(state["hosts"]["alpha"]["last_imported"], 2);
    assert!(!sb.calls()[previous_calls..].contains("ssh "));
}

#[test]
fn local_import_requires_mirrors_and_does_not_invent_a_pull_checkpoint() {
    let sb = Sandbox::new("local-import-new");
    touch(
        &sb.root().join("alpha/codex/sessions/session.jsonl"),
        DAY / 24,
    );

    let out = sb.ccr(&["import", "alpha", "missing"], &[]);

    assert!(!out.status.success());
    let state = sb.state();
    assert_eq!(state["hosts"]["alpha"]["last_epoch"], 0);
    assert_eq!(state["hosts"]["alpha"]["last_imported"], 2);
    assert_eq!(state["hosts"]["missing"]["failures"], 1);
    assert!(!sb.calls().contains("ssh "));
}

#[test]
fn quiet_sync_reports_import_failures_with_nonzero_exit() {
    let sb = Sandbox::new("quiet-import-failure");
    seed_remote(&sb, "alpha");

    let out = sb.ccr(
        &["sync", "alpha", "--quiet"],
        &[("FAKE_CC_FAIL_IMPORT", "1")],
    );

    assert!(!out.status.success(), "定时任务必须能观察到导入失败");
    assert_eq!(sb.state()["hosts"]["alpha"]["last_epoch"], 0);
    assert_eq!(sb.state()["hosts"]["alpha"]["failures"], 1);
}

fn mirrored_jsonl(dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            paths.extend(mirrored_jsonl(&entry.path()));
        } else if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            paths.push(entry.path());
        }
    }
    paths
}

#[test]
fn extra_codex_roots_require_configuration_and_backfill_old_actor_logs() {
    let sb = Sandbox::new("extra-roots");
    seed_remote(&sb, "alpha");
    let extra = sb.remote("alpha").join("Pharos 'test'/runtime/projects");
    for actor in ["main", "workers/worker1"] {
        touch(
            &extra
                .join(actor)
                .join(".codex-home/sessions/2026/10/01/same-name.jsonl"),
            DAY * 5,
        );
    }
    touch(
        &extra.join("materials/copy/.codex-home/sessions/copied.jsonl"),
        DAY / 24,
    );
    fs::write(
        extra.join("main/.codex-home/auth.json"),
        "private authentication",
    )
    .unwrap();
    let first = sb.ccr(&["sync", "alpha"], &[]);
    assert!(first.status.success(), "{}", text(&first));
    let mirror = sb.root().join("alpha/codex/sessions");
    assert_eq!(mirrored_jsonl(&mirror).len(), 1);

    let configured = sb.ccr(
        &[
            "config",
            "--add-codex-root",
            &format!("alpha={}", extra.display()),
        ],
        &[],
    );
    assert!(configured.status.success(), "{}", text(&configured));
    let synced = sb.ccr(&["sync", "alpha"], &[]);

    assert!(synced.status.success(), "{}", text(&synced));
    let files = mirrored_jsonl(&mirror);
    let extra_files: Vec<_> = files
        .iter()
        .filter(|p| p.file_name().unwrap() == "same-name.jsonl")
        .collect();
    assert_eq!(extra_files.len(), 2, "不同 actor 的同名日志都必须保留");
    assert_ne!(extra_files[0].parent(), extra_files[1].parent());
    for p in extra_files {
        assert!(
            p.strip_prefix(&mirror).unwrap().components().count() <= 4,
            "导入器只扫描三层子目录"
        );
    }
    assert!(!files
        .iter()
        .any(|p| p.file_name().unwrap() == "copied.jsonl"));
    assert!(!mirror.join("auth.json").exists());
}

#[test]
fn failed_extra_root_import_keeps_the_initial_backfill_pending() {
    let sb = Sandbox::new("extra-root-retry");
    seed_remote(&sb, "alpha");
    assert!(sb.ccr(&["sync", "alpha"], &[]).status.success());
    let checkpoint = sb.state()["hosts"]["alpha"]["last_epoch"].clone();
    let extra = sb.remote("alpha").join("orchestration");
    touch(&extra.join("one/.codex-home/sessions/first.jsonl"), DAY * 5);
    assert!(sb
        .ccr(
            &[
                "config",
                "--add-codex-root",
                &format!("alpha={}", extra.display())
            ],
            &[],
        )
        .status
        .success());

    let failed = sb.ccr(&["sync", "alpha"], &[("FAKE_CC_FAIL_IMPORT", "1")]);
    assert!(!failed.status.success());
    assert_eq!(sb.state()["hosts"]["alpha"]["last_epoch"], checkpoint);
    touch(
        &extra.join("two/.codex-home/sessions/second.jsonl"),
        DAY * 5,
    );
    let retried = sb.ccr(&["sync", "alpha"], &[]);

    assert!(retried.status.success(), "{}", text(&retried));
    let files = mirrored_jsonl(&sb.root().join("alpha/codex/sessions"));
    assert!(files
        .iter()
        .any(|p| p.file_name().unwrap() == "second.jsonl"));
}

#[test]
fn configured_extra_only_host_is_included_in_automatic_sync() {
    let sb = Sandbox::new("extra-only-auto");
    let extra = sb.remote("alpha").join("research");
    touch(
        &extra.join("worker/.codex-home/sessions/old.jsonl"),
        DAY * 5,
    );
    let configured = sb.ccr(
        &[
            "config",
            "--add-codex-root",
            &format!("alpha={}", extra.display()),
        ],
        &[],
    );
    assert!(configured.status.success(), "{}", text(&configured));

    let out = sb.ccr(&["sync", "--all", "--quiet"], &[]);

    assert!(out.status.success(), "{}", text(&out));
    assert!(sb.state()["hosts"]["alpha"]["last_epoch"].as_i64().unwrap() > 0);
    let files = mirrored_jsonl(&sb.root().join("alpha/codex/sessions"));
    assert!(files.iter().any(|p| p.file_name().unwrap() == "old.jsonl"));
}

#[test]
fn all_hosts_unreachable_is_reported_and_backoff_recorded() {
    let sb = Sandbox::new("unreachable");
    sb.ssh_config("Host deadbox-a\n  HostName 10.0.0.1\nHost deadbox-b\n  HostName 10.0.0.2\n");

    let out = sb.ccr(&["sync", "--all", "--quiet"], &[]);

    assert!(!out.status.success(), "全部连不上要让定时任务日志看得到");
    assert!(text(&out).contains("全部连不上"), "{}", text(&out));
    let state = sb.state();
    for h in ["deadbox-a", "deadbox-b"] {
        assert_eq!(state["hosts"][h]["failures"], 1, "退出前也要把退避记下来");
    }
}

#[test]
fn aliases_of_one_machine_are_synced_once() {
    let sb = Sandbox::new("alias");
    seed_remote(&sb, "mac");
    sb.ssh_config("Host mac mac-lan\n  HostName mac.local\n");

    let probe = sb.ccr(&["hosts", "--probe", "--json"], &[]);
    assert!(probe.status.success(), "{}", text(&probe));
    let hosts: Value = serde_json::from_slice(&probe.stdout).unwrap();
    let find = |h: &str| {
        hosts
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["host"] == h)
            .cloned()
            .unwrap()
    };
    assert_eq!(find("mac-lan")["duplicate_of"], "mac");
    assert!(find("mac")["duplicate_of"].is_null());

    let out = sb.ccr(&["sync", "--all", "--quiet"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let calls = sb.calls();
    assert!(calls.contains("ssh pull mac\n"));
    assert!(
        !calls.contains("ssh pull mac-lan"),
        "同一份日志只拉一次：\n{calls}"
    );
}

/// 远端命令被杀后，ssh 通道不会关闭（管道半死），本地 tar 永远等不到 EOF。
/// 这种情况下 ccr 必须靠"多久没有新字节"自己收场：
/// 以前只能等满 60 分钟的总时长上限，整轮同步和后续定时任务全被一台主机拖住。
#[test]
fn stalled_transfer_is_aborted_instead_of_hanging_the_run() {
    let sb = Sandbox::new("stall");
    seed_remote(&sb, "stallbox");

    let (out, timed_out) = sb.ccr_with_timeout(
        &["sync", "stallbox"],
        &[("CCR_TRANSFER_IDLE_SECS", "2")],
        Duration::from_secs(60),
    );
    assert!(
        !timed_out,
        "传输停滞必须自己收场，不能一直挂着：\n{}",
        text(&out)
    );
    assert!(!out.status.success(), "停滞要算失败：\n{}", text(&out));
    assert!(
        text(&out).contains("传输停滞"),
        "要明确告诉用户是停滞而不是别的错误：\n{}",
        text(&out)
    );

    // 失败要落进 state：下一轮这台机器进退避，不会再把整轮拖住
    let state = sb.state();
    let err = state["hosts"]["stallbox"]["last_error"]
        .as_str()
        .unwrap_or_default();
    assert!(err.contains("传输停滞"), "停滞原因要记进 state：{err}");
    assert_eq!(
        state["hosts"]["stallbox"]["failures"].as_u64(),
        Some(1),
        "停滞算一次失败，进入退避"
    );
}

/// 传输停滞不能推进增量断点：拉了一半的日志不算同步成功，下轮还要重来。
#[test]
fn stalled_transfer_does_not_advance_the_checkpoint() {
    let sb = Sandbox::new("stall-cp");
    seed_remote(&sb, "stallbox");

    let (out, timed_out) = sb.ccr_with_timeout(
        &["sync", "stallbox"],
        &[("CCR_TRANSFER_IDLE_SECS", "2")],
        Duration::from_secs(60),
    );
    assert!(!timed_out);
    assert!(!out.status.success());

    let state = sb.state();
    assert_eq!(
        state["hosts"]["stallbox"]["last_epoch"].as_i64(),
        Some(0),
        "失败不能推进断点"
    );
}
