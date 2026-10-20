//! SSH 主机自动发现与探测。

use crate::{home, load_config};
use std::collections::BTreeMap;
use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

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
    pub err: Option<String>,
}

impl Probe {
    pub fn active(&self) -> bool {
        self.ok && (self.codex + self.claude) > 0
    }
}

/// 发现候选主机：配置 > ~/.ssh/config > CC Switch Fleet
pub fn discover() -> Vec<HostSpec> {
    let mut map: BTreeMap<String, HostSpec> = BTreeMap::new();
    let ssh_hosts = ssh_config_hosts();
    let git_aliases: std::collections::HashSet<String> = ssh_hosts
        .iter()
        .filter(|(_, hostname, user)| is_git_remote(hostname, user))
        .map(|(alias, _, _)| alias.clone())
        .collect();

    for h in load_config().hosts {
        map.insert(
            h.clone(),
            HostSpec {
                id: h,
                source: "config".into(),
            },
        );
    }

    for (id, _, _) in &ssh_hosts {
        if git_aliases.contains(id) {
            continue;
        }
        map.entry(id.clone()).or_insert(HostSpec {
            id: id.clone(),
            source: "ssh".into(),
        });
    }

    for id in fleet_hosts() {
        if git_aliases.contains(&id) {
            continue;
        }
        map.entry(id.clone()).or_insert(HostSpec {
            id,
            source: "fleet".into(),
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

/// 解析 ~/.ssh/config 里的 Host 别名，返回 (别名, HostName, User)。
fn ssh_config_hosts() -> Vec<(String, String, String)> {
    let path = home().join(".ssh").join("config");
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };

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

const PROBE_SCRIPT: &str = r#"
c=$(find "$HOME/.codex/sessions" -type f -name '*.jsonl' -newermt '-3 days' 2>/dev/null | wc -l)
l=$(find "$HOME/.claude/projects" -type f -name '*.jsonl' -newermt '-3 days' 2>/dev/null | wc -l)
s=$(du -sh "$HOME/.codex/sessions" 2>/dev/null | cut -f1)
echo "__CCR__ codex=$c claude=$l size=${s:-0}"
"#;

pub fn probe(host: &str) -> Probe {
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=2",
        ])
        .arg(host)
        .arg(PROBE_SCRIPT)
        .output();

    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            let mut p = Probe {
                ok: true,
                ..Default::default()
            };
            if let Some(line) = text.lines().find(|l| l.starts_with("__CCR__")) {
                for tok in line.trim_start_matches("__CCR__").split_whitespace() {
                    if let Some((k, v)) = tok.split_once('=') {
                        match k {
                            "codex" => p.codex = v.parse().unwrap_or(0),
                            "claude" => p.claude = v.parse().unwrap_or(0),
                            "size" => p.size = v.to_string(),
                            _ => {}
                        }
                    }
                }
            }
            p
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr)
                .lines()
                .last()
                .unwrap_or("ssh 失败")
                .to_string();
            Probe {
                ok: false,
                err: Some(err),
                ..Default::default()
            }
        }
        Err(e) => Probe {
            ok: false,
            err: Some(e.to_string()),
            ..Default::default()
        },
    }
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
