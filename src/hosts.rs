//! SSH 主机自动发现、探测与去重。

use crate::ssh;
use crate::state::{home, load_config};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct HostSpec {
    pub id: String,
    /// config / ssh / fleet
    pub source: String,
}

#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub ok: bool,
    /// 近 3 天 Codex 会话文件数
    pub codex: usize,
    /// 近 3 天 Claude 会话文件数
    pub claude: usize,
    /// Codex sessions 目录体积
    pub size: String,
    /// 近 3 天会话文件路径列表的校验和：同一台机器同一用户 → 相同；
    /// 不同机器的文件名含 UUID，不会撞。没有近期文件时为 None（不参与去重）
    pub fingerprint: Option<String>,
    pub err: Option<String>,
}

impl Probe {
    pub fn active(&self) -> bool {
        self.ok && (self.codex + self.claude) > 0
    }
}

/// 主机名同时用作 ssh 目标和本机镜像目录名：拒绝以 `-` 开头（会被 ssh 当成选项）、
/// 含路径分隔符或纯点号（会逃出镜像根目录），以及含空白 / 控制字符的名字。
pub fn valid_host_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.chars().all(|c| c == '.')
        && !id
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == '\\')
}

/// 发现候选主机：配置 > ~/.ssh/config > CC Switch Fleet
pub fn discover() -> Vec<HostSpec> {
    let mut map: BTreeMap<String, HostSpec> = BTreeMap::new();
    let ssh_hosts = ssh_config_hosts();
    let git_aliases: HashSet<String> = ssh_hosts
        .iter()
        .filter(|(_, hostname, user)| is_git_remote(hostname, user))
        .map(|(alias, _, _)| alias.clone())
        .collect();

    for h in load_config().hosts {
        if valid_host_id(&h) {
            map.insert(
                h.clone(),
                HostSpec {
                    id: h,
                    source: "config".into(),
                },
            );
        }
    }

    let discovered = ssh_hosts
        .iter()
        .map(|(id, _, _)| (id.clone(), "ssh"))
        .chain(fleet_hosts().into_iter().map(|id| (id, "fleet")));
    for (id, source) in discovered {
        if !valid_host_id(&id) || git_aliases.contains(&id) {
            continue;
        }
        map.entry(id.clone()).or_insert(HostSpec {
            id,
            source: source.into(),
        });
    }

    map.into_values().collect()
}

fn is_git_remote(hostname: &str, user: &str) -> bool {
    hostname.eq_ignore_ascii_case("github.com")
        || hostname.eq_ignore_ascii_case("gitlab.com")
        || hostname.eq_ignore_ascii_case("codeberg.org")
        || user == "git"
}

/// 读取 ~/.ssh/config 里的 Host 别名，返回 (别名, HostName, User)。
fn ssh_config_hosts() -> Vec<(String, String, String)> {
    fs::read_to_string(home().join(".ssh").join("config"))
        .map(|text| parse_ssh_config(&text))
        .unwrap_or_default()
}

fn parse_ssh_config(text: &str) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut hostname = String::new();
    let mut user = String::new();

    let flush = |names: &mut Vec<String>,
                 hostname: &mut String,
                 user: &mut String,
                 out: &mut Vec<(String, String, String)>| {
        for n in names.drain(..) {
            if n.contains('*') || n.contains('?') || n.contains('!') {
                continue;
            }
            out.push((n, hostname.clone(), user.clone()));
        }
        hostname.clear();
        user.clear();
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(key) = it.next() else { continue };
        let vals: Vec<&str> = it.collect();
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                flush(&mut names, &mut hostname, &mut user, &mut out);
                names = vals.iter().map(|s| s.to_string()).collect();
            }
            "hostname" => {
                if let Some(v) = vals.first() {
                    hostname = v.to_string();
                }
            }
            "user" => {
                if let Some(v) = vals.first() {
                    user = v.to_string();
                }
            }
            _ => {}
        }
    }
    flush(&mut names, &mut hostname, &mut user, &mut out);

    out.sort();
    out.dedup();
    out
}

/// 复用 CC Switch Fleet 的主机发现（它已经解析了 Codex App 的 SSH 列表）。
fn fleet_hosts() -> Vec<String> {
    let candidates = [
        "cc-fleet",
        "/Applications/CC Switch Fleet.app/Contents/MacOS/cc-fleet",
    ];
    for bin in candidates {
        let Ok(out) = Command::new(bin).arg("hosts").output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            continue;
        };
        let mut ids = Vec::new();
        if let Some(arr) = v["data"]["hosts"].as_array() {
            for h in arr {
                if let Some(id) = h["id"].as_str() {
                    ids.push(id.to_string());
                }
            }
        }
        return ids;
    }
    Vec::new()
}

// ---------------------------------------------------------------- 探测

/// 指纹脚本：远端探测与本机自检共用同一段，保证算法一致。
/// `LC_ALL=C` 让排序与 locale 无关（ssh 会把本机的 LANG 带过去）。
macro_rules! fingerprint_sh {
    () => {
        r#"f=$(find -H "$HOME/.codex/sessions" "$HOME/.claude/projects" -type f -name '*.jsonl' -mtime -3 2>/dev/null | LC_ALL=C sort)
if [ -n "$f" ]; then fp=$(printf '%s\n' "$f" | cksum | cut -d' ' -f1); else fp=none; fi
"#
    };
}

/// 只用 POSIX 写法，macOS 远端（BSD 工具）、BusyBox 同样适用：
/// - `-mtime -3`：`-newermt` 是 GNU/BSD 扩展，BusyBox 没有；
/// - `-H`：会话目录本身是符号链接（挪到别的盘）时也要跟进去；
/// - `$((c))` / `awk '{print $1}'`：BSD `wc -l`、`du -sh` 的输出带前导空格，
///   原样拼进 `codex=…` 会被按空白切开，解析成 0 / 空。
const PROBE_SCRIPT: &str = concat!(
    fingerprint_sh!(),
    r#"c=$(find -H "$HOME/.codex/sessions" -type f -name '*.jsonl' -mtime -3 2>/dev/null | wc -l)
l=$(find -H "$HOME/.claude/projects" -type f -name '*.jsonl' -mtime -3 2>/dev/null | wc -l)
s=$(du -H -sh "$HOME/.codex/sessions" 2>/dev/null | awk '{print $1}')
echo "__CCR__ codex=$((c)) claude=$((l)) size=${s:-0} fp=$fp"
"#
);

const LOCAL_FP_SCRIPT: &str = concat!(fingerprint_sh!(), "echo \"$fp\"\n");

/// 单台探测的总时长上限：正常几秒，远端 find / du 卡住时不能一直占着运行锁
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

pub fn probe(host: &str) -> Probe {
    let child = match ssh::spawn_script(host, &ssh::PROBE, PROBE_SCRIPT, Stdio::piped()) {
        Ok(c) => c,
        Err(e) => {
            return Probe {
                err: Some(format!("无法启动 ssh: {e}")),
                ..Default::default()
            }
        }
    };
    let watchdog = ssh::Watchdog::arm(child.id(), PROBE_TIMEOUT);
    let out = child.wait_with_output();
    let timed_out = watchdog.disarm();
    match out {
        Ok(o) if timed_out && o.status.code().is_none() => Probe {
            err: Some(format!(
                "探测超过 {} 秒没有返回，已中止",
                PROBE_TIMEOUT.as_secs()
            )),
            ..Default::default()
        },
        Ok(o) if o.status.success() => parse_probe(&String::from_utf8_lossy(&o.stdout)),
        Ok(o) => Probe {
            err: Some(ssh::describe_exit(
                o.status.code(),
                &String::from_utf8_lossy(&o.stderr),
            )),
            ..Default::default()
        },
        Err(e) => Probe {
            err: Some(format!("等待 ssh 退出失败: {e}")),
            ..Default::default()
        },
    }
}

fn parse_probe(stdout: &str) -> Probe {
    let mut p = Probe {
        ok: true,
        ..Default::default()
    };
    if let Some(line) = stdout.lines().find(|l| l.starts_with("__CCR__")) {
        for tok in line.trim_start_matches("__CCR__").split_whitespace() {
            if let Some((k, v)) = tok.split_once('=') {
                match k {
                    "codex" => p.codex = v.parse().unwrap_or(0),
                    "claude" => p.claude = v.parse().unwrap_or(0),
                    "size" => p.size = v.to_string(),
                    "fp" if !v.is_empty() && v != "none" => p.fingerprint = Some(v.to_string()),
                    _ => {}
                }
            }
        }
    }
    p
}

/// 并行探测（最多 8 个并发），结果按主机名排序。
pub fn probe_many(hosts: &[String]) -> Vec<(String, Probe)> {
    if hosts.is_empty() {
        return Vec::new();
    }
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(String, Probe)>> = Mutex::new(Vec::with_capacity(hosts.len()));
    let workers = 8.min(hosts.len());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= hosts.len() {
                    break;
                }
                let p = probe(&hosts[i]);
                results.lock().unwrap().push((hosts[i].clone(), p));
            });
        }
    });

    let mut v = results.into_inner().unwrap_or_default();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// 并行探测，并在探测前后各取一次本机指纹。指纹覆盖的文件列表随时间变化，
/// 只取一次的话，探测期间新建一个会话就对不上；前后两次任一相同即视为本机。
pub fn probe_with_local(hosts: &[String]) -> (Vec<(String, Probe)>, Vec<String>) {
    let before = local_fingerprint();
    let results = probe_many(hosts);
    let after = local_fingerprint();
    (results, before.into_iter().chain(after).collect())
}

// ---------------------------------------------------------------- 去重

#[derive(Debug, Clone, PartialEq)]
pub enum Skip {
    /// 与本机是同一份数据（CC Switch 本来就在扫本机日志）
    Local,
    /// 与另一个别名是同一份数据
    SameAs(String),
}

impl Skip {
    pub fn describe(&self) -> String {
        match self {
            Skip::Local => "就是本机，CC Switch 已直接统计".to_string(),
            Skip::SameAs(h) => format!("与 {h} 是同一份日志"),
        }
    }
}

/// 本机会话数据的指纹，用来认出"其实就是本机"的别名（共享 dotfiles 时很常见）。
fn local_fingerprint() -> Option<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(LOCAL_FP_SCRIPT)
        .output()
        .ok()?;
    let fp = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !fp.is_empty() && fp != "none").then_some(fp)
}

/// 来源优先级：手动配置 > ssh config > Fleet
pub fn source_rank(specs: &[HostSpec], host: &str) -> u8 {
    match specs
        .iter()
        .find(|s| s.id == host)
        .map(|s| s.source.as_str())
    {
        Some("config") => 0,
        Some("ssh") => 1,
        Some("fleet") => 2,
        _ => 3,
    }
}

/// 同一份会话数据只同步一次，返回应跳过的主机及原因。
///
/// 多个别名（如 `macmini` / `macmini-lan` / `macmini-tailscale`）指向同一台机器的同一用户时
/// 指纹相同，只保留优先级最高的一个（来源 > 名字短 > 字典序），否则镜像、带宽和
/// `ccr report` 合计都会翻倍；与本机指纹相同的直接跳过。
/// 没有指纹（没有近期日志 / 不可达）的不参与去重。
///
/// 漏判（指纹恰好对不上）只会多拉一份：CC Switch 按记录内容（线程 + 序号 / message.id）
/// 去重，`ccr report` 也跨主机去重，都不会重复计数；误判才会丢数据，所以宁可漏判。
pub fn duplicates(
    results: &[(String, Probe)],
    local_fps: &[String],
    rank: impl Fn(&str) -> u8,
) -> BTreeMap<String, Skip> {
    let preference = |h: &str| (rank(h), h.len(), h.to_string());
    let mut best: HashMap<&str, &str> = HashMap::new();
    for (h, p) in results {
        if let Some(fp) = p.fingerprint.as_deref() {
            if best
                .get(fp)
                .is_none_or(|cur| preference(h) < preference(cur))
            {
                best.insert(fp, h);
            }
        }
    }

    results
        .iter()
        .filter_map(|(h, p)| {
            let fp = p.fingerprint.as_deref()?;
            if local_fps.iter().any(|l| l == fp) {
                Some((h.clone(), Skip::Local))
            } else if best[fp] != h.as_str() {
                Some((h.clone(), Skip::SameAs(best[fp].to_string())))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::{scratch, touch};
    use std::path::Path;

    const DAY: Duration = Duration::from_secs(86_400);

    #[test]
    fn host_ids_are_safe_for_ssh_and_paths() {
        for ok in [
            "netcup",
            "root@1.2.3.4",
            "macmini-tailscale",
            "a.b_c:22",
            "我的服务器",
        ] {
            assert!(valid_host_id(ok), "{ok} 应合法");
        }
        for bad in [
            "",
            "-oProxyCommand=touch /tmp/x",
            "../etc",
            "a/b",
            ".",
            "..",
            "a b",
            "x\ty",
        ] {
            assert!(!valid_host_id(bad), "{bad:?} 应被拒绝");
        }
    }

    #[test]
    fn ssh_config_parsing_keeps_concrete_aliases() {
        let text = "\
Include ~/.ssh/config.d/*.conf
Host gh-work
  HostName github.com
  User git
# comment
Host mini mini-lan
  HostName mini.example.lan
  User alice
Host *.internal !bastion
  User ops
";
        let hosts = parse_ssh_config(text);
        assert_eq!(
            hosts,
            vec![
                ("gh-work".into(), "github.com".into(), "git".into()),
                ("mini".into(), "mini.example.lan".into(), "alice".into()),
                ("mini-lan".into(), "mini.example.lan".into(), "alice".into()),
            ]
        );
        assert!(is_git_remote(&hosts[0].1, &hosts[0].2));
    }

    #[test]
    fn parse_probe_reads_marker_line() {
        let p = parse_probe("motd noise\n__CCR__ codex=12 claude=0 size=228M fp=3141592653\n");
        assert!(p.ok && p.active());
        assert_eq!((p.codex, p.claude), (12, 0));
        assert_eq!(p.size, "228M");
        assert_eq!(p.fingerprint.as_deref(), Some("3141592653"));

        let idle = parse_probe("__CCR__ codex=0 claude=0 size=0 fp=none\n");
        assert!(idle.ok && !idle.active());
        assert_eq!(idle.fingerprint, None);
    }

    fn run_sh(script: &str, home: &Path) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn probe_script_counts_recent_logs_portably() {
        // 直接用本机 sh 跑同一段脚本：macOS 上就是 BSD find / wc，Linux CI 上是 GNU
        let home = scratch("probe-home");
        touch(
            &home.join(".codex/sessions/2026/10/21/rollout-a.jsonl"),
            DAY / 24,
        );
        touch(
            &home.join(".codex/sessions/2026/10/21/rollout-b.jsonl"),
            DAY / 24,
        );
        touch(
            &home.join(".codex/sessions/2026/09/01/rollout-old.jsonl"),
            DAY * 10,
        );
        touch(&home.join(".claude/projects/p/s.jsonl"), DAY / 24);

        let p = parse_probe(&run_sh(PROBE_SCRIPT, &home));

        assert_eq!(
            (p.codex, p.claude),
            (2, 1),
            "只数近 3 天的（BSD wc -l 输出带前导空格）"
        );
        assert!(p.fingerprint.is_some());
        assert!(
            !p.size.is_empty() && p.size != "0",
            "BSD du -sh 小体积时同样有前导空格，size 不能被解析丢: {:?}",
            p.size
        );
    }

    #[test]
    fn probe_follows_a_symlinked_sessions_dir() {
        let home = scratch("probe-symlink-home");
        let real = scratch("probe-symlink-real");
        touch(&real.join("2026/10/21/rollout-a.jsonl"), DAY / 24);
        fs::create_dir_all(home.join(".codex")).unwrap();
        std::os::unix::fs::symlink(&real, home.join(".codex/sessions")).unwrap();

        let p = parse_probe(&run_sh(PROBE_SCRIPT, &home));

        assert_eq!(
            p.codex, 1,
            "~/.codex/sessions 是符号链接（挂到别的盘）时也要数到"
        );
        assert!(p.fingerprint.is_some());
        // 不加 -H 时 du 量的是链接本身：BSD 输出 0B，GNU 输出 0
        assert!(
            !["", "0", "0B"].contains(&p.size.as_str()),
            "体积要量链接指向的目录: {:?}",
            p.size
        );
    }

    #[test]
    fn fingerprint_identifies_the_same_data_only() {
        let a = scratch("fp-a");
        let b = scratch("fp-b");
        let empty = scratch("fp-empty");
        touch(&a.join(".codex/sessions/x.jsonl"), DAY / 24);
        touch(&b.join(".codex/sessions/x.jsonl"), DAY / 24);

        let fp = |home: &Path| run_sh(LOCAL_FP_SCRIPT, home).trim().to_string();

        assert_eq!(fp(&a), fp(&a), "同一份数据指纹稳定");
        assert_ne!(fp(&a), fp(&b), "不同 HOME 的数据不能撞");
        assert_eq!(fp(&empty), "none", "没有近期日志时不给指纹");
    }

    fn active(fp: Option<&str>) -> Probe {
        Probe {
            ok: true,
            codex: 1,
            fingerprint: fp.map(str::to_string),
            ..Default::default()
        }
    }

    fn plan(skip: &BTreeMap<String, Skip>) -> Vec<(&str, Skip)> {
        skip.iter().map(|(h, s)| (h.as_str(), s.clone())).collect()
    }

    #[test]
    fn duplicates_keep_one_alias_per_fingerprint() {
        let results = vec![
            ("idle".to_string(), active(None)),
            ("idle2".to_string(), active(None)),
            ("macmini".to_string(), active(Some("111"))),
            ("macmini-lan".to_string(), active(Some("111"))),
            ("macmini-tailscale".to_string(), active(Some("111"))),
            ("netcup".to_string(), active(Some("222"))),
        ];
        let skip = duplicates(&results, &[], |_| 1);
        assert_eq!(
            plan(&skip),
            [
                ("macmini-lan", Skip::SameAs("macmini".into())),
                ("macmini-tailscale", Skip::SameAs("macmini".into())),
            ],
            "名字最短的别名留下；没有指纹的不参与"
        );
    }

    #[test]
    fn duplicates_prefer_configured_alias_and_skip_local() {
        let results = vec![
            ("me".to_string(), active(Some("9"))),
            ("mm".to_string(), active(Some("1"))),
            ("pinned-mac".to_string(), active(Some("1"))),
        ];
        // 本机指纹在探测前后各取一次，命中任意一个都算本机
        let local = ["8".to_string(), "9".to_string()];
        let skip = duplicates(&results, &local, |h| if h == "pinned-mac" { 0 } else { 1 });
        assert_eq!(
            plan(&skip),
            [
                ("me", Skip::Local),
                ("mm", Skip::SameAs("pinned-mac".into())),
            ]
        );
    }

    #[test]
    fn source_rank_orders_config_first() {
        let specs = vec![
            HostSpec {
                id: "a".into(),
                source: "fleet".into(),
            },
            HostSpec {
                id: "b".into(),
                source: "config".into(),
            },
            HostSpec {
                id: "c".into(),
                source: "ssh".into(),
            },
        ];
        assert!(source_rank(&specs, "b") < source_rank(&specs, "c"));
        assert!(source_rank(&specs, "c") < source_rank(&specs, "a"));
        assert!(source_rank(&specs, "a") < source_rank(&specs, "unknown"));
    }
}
