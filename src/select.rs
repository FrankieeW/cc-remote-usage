//! 本轮同步哪些主机：命令行点名 / 定时任务列表 / 配置 / 自动发现，再叠加退避冷却与去重。

use crate::fmt_epoch;
use crate::hosts;
use crate::state::{cooling_until, record_failure, Config, State};
use anyhow::{bail, Result};
use std::io::IsTerminal;

/// 主机名会被当作 ssh 参数和镜像目录名，入口处统一校验。
pub fn check_hosts(hosts: &[String]) -> Result<()> {
    if let Some(bad) = hosts.iter().find(|h| !hosts::valid_host_id(h)) {
        bail!("主机名 `{bad}` 不合法（不能以 - 开头，不能含空白、/ 或 \\）");
    }
    Ok(())
}

/// 逗号分隔的主机列表（空串得到空列表）。
pub fn parse_host_list(s: &str) -> Result<Vec<String>> {
    let hosts: Vec<String> = s
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect();
    check_hosts(&hosts)?;
    Ok(hosts)
}

/// 本次的初始主机列表，以及是否算"点名"。只有命令行点名才无视冷却（用户明确要求重试）；
/// REMOTE_USAGE_HOSTS 来自定时任务，仍要受退避约束，否则死机器每轮都会赔超时。
pub fn initial_hosts(cli: Vec<String>, env: Option<String>) -> (Vec<String>, bool) {
    if !cli.is_empty() {
        return (cli, true);
    }
    let from_env = env
        .map(|list| list.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    (from_env, false)
}

/// 一轮探测的结果
pub struct Probed {
    /// 可达、有近期日志、且不是重复别名 / 本机的主机
    pub active: Vec<(String, hosts::Probe)>,
    /// 实际发起探测的台数（冷却中的不算）
    pub probed: usize,
    /// 连不上的主机及原因
    pub unreachable: Vec<(String, String)>,
}

impl Probed {
    /// 探测了至少一台，而且全部连不上（多半是本机网络 / SSH agent 的问题，而不是服务器都挂了）
    pub fn all_unreachable(&self) -> bool {
        self.probed > 0 && self.unreachable.len() == self.probed
    }
}

/// 探测候选主机：跳过冷却中的（避免反复赔超时），把探测失败计入退避，
/// 再去掉指向同一份数据的别名和本机。
pub fn probe_candidates(
    cfg: &Config,
    state: &mut State,
    quiet: bool,
    respect_backoff: bool,
) -> Result<Probed> {
    let specs = hosts::discover();
    if specs.is_empty() {
        bail!("没发现任何 SSH 主机，用 `ccr config --set-hosts a,b` 指定");
    }

    let mut ids: Vec<String> = Vec::new();
    let mut cooling = 0usize;
    for s in &specs {
        if respect_backoff {
            if let Some(until) = cooling_until(state, &s.id) {
                cooling += 1;
                if !quiet {
                    eprintln!("  跳过 {}（冷却中，{} 再试）", s.id, fmt_epoch(until));
                }
                continue;
            }
        }
        ids.push(s.id.clone());
    }

    if ids.is_empty() {
        if !quiet {
            eprintln!("候选主机都在退避冷却中（{cooling} 台），立即重试用 --retry-now");
        }
        return Ok(Probed {
            active: Vec::new(),
            probed: 0,
            unreachable: Vec::new(),
        });
    }

    if !quiet {
        eprintln!("正在探测 {} 台候选主机…", ids.len());
    }
    let (results, local_fps) = hosts::probe_with_local(&ids);
    let mut unreachable = Vec::new();
    for (h, p) in &results {
        if !p.ok {
            let err = p.err.as_deref().unwrap_or("ssh 失败");
            record_failure(cfg, state, h, err);
            unreachable.push((h.clone(), err.to_string()));
        }
    }

    let dups = hosts::duplicates(&results, &local_fps, |h| hosts::source_rank(&specs, h));
    if !quiet {
        for (h, why) in &dups {
            eprintln!("  跳过 {h}（{}）", why.describe());
        }
    }
    let probed = results.len();
    let active = results
        .into_iter()
        .filter(|(h, p)| p.active() && !dups.contains_key(h))
        .collect();
    Ok(Probed {
        active,
        probed,
        unreachable,
    })
}

/// 能弹交互多选框（stdin / stdout 都是终端）
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// 交互终端里多选（默认全选），否则直接全选。
pub fn pick_hosts(active: &[(String, hosts::Probe)]) -> Result<Vec<String>> {
    if !interactive() {
        return Ok(active.iter().map(|(h, _)| h.clone()).collect());
    }
    let labels: Vec<String> = active
        .iter()
        .map(|(h, p)| {
            format!(
                "{:<22} codex {:>3} 文件  claude {:>3} 文件  {}",
                h,
                p.codex,
                p.claude,
                if p.size.is_empty() { "-" } else { &p.size }
            )
        })
        .collect();
    let picked = dialoguer::MultiSelect::new()
        .with_prompt("空格选择要同步的服务器，回车确认")
        .items(&labels)
        .defaults(&vec![true; active.len()])
        .interact()?;
    Ok(picked.into_iter().map(|i| active[i].0.clone()).collect())
}

/// 去掉冷却中的主机，非 quiet 时说明原因。
pub fn skip_cooling(state: &State, hosts: Vec<String>, quiet: bool) -> Vec<String> {
    let (cooled, ready): (Vec<String>, Vec<String>) = hosts
        .into_iter()
        .partition(|h| cooling_until(state, h).is_some());
    if !quiet && !cooled.is_empty() {
        println!("跳过 {} 台冷却中的主机：", cooled.len());
        for h in &cooled {
            let failures = state.hosts.get(h).map(|s| s.failures).unwrap_or(0);
            let until = cooling_until(state, h).unwrap_or_default();
            println!(
                "  - {h}（连续失败 {failures} 次，{} 再试）",
                fmt_epoch(until)
            );
        }
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_command_line_hosts_bypass_backoff() {
        let (hosts, explicit) = initial_hosts(vec!["netcup".into()], Some("a b".into()));
        assert_eq!((hosts, explicit), (vec!["netcup".to_string()], true));

        let (hosts, explicit) = initial_hosts(Vec::new(), Some(" netcup  nuyek-yeats ".into()));
        assert_eq!(hosts, ["netcup", "nuyek-yeats"]);
        assert!(!explicit, "定时任务传进来的主机列表仍要受退避约束");

        assert_eq!(initial_hosts(Vec::new(), None), (Vec::new(), false));
    }

    #[test]
    fn host_lists_are_trimmed_and_validated() {
        assert_eq!(
            parse_host_list(" netcup, frankie-vps ,").unwrap(),
            ["netcup", "frankie-vps"]
        );
        assert!(parse_host_list("").unwrap().is_empty(), "空串用于清空");
        assert!(parse_host_list("ok,-oProxyCommand=x").is_err());
        assert!(parse_host_list("../escape").is_err());
    }
}
