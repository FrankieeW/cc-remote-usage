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
use std::process::{Command, Output};
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
printf '%s\n' "$script" | HOME="$FAKE_REMOTES/$home" "$@"
"#;

/// 假 cc-switch：记录作用域环境变量与参数，回一份 CC Switch 5.x 形状的 JSON。
const FAKE_CC_SWITCH: &str = r#"#!/bin/sh
echo "cc-switch CODEX_HOME=$CODEX_HOME CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR $*" >> "$FAKE_LOG"
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
    assert!(out.status.success(), "{}", text(&out));

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

    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("解析失败"));
    assert!(stale.exists(), "配置读不出来时不能按默认值删文件");
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
