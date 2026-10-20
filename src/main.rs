//! ccr — 把服务器上 Codex / Claude Code agent 的用量同步进 CC Switch。
//!
//! CC Switch 的「用量」看板只扫本机的会话日志（`~/.codex/sessions`、
//! `~/.claude/projects`，受 `CODEX_HOME` / `CLAUDE_CONFIG_DIR` 控制）。
//! 本工具把各服务器上的会话日志增量拉回本机镜像目录，再用作用域环境变量
//! 调 `cc-switch sessions sync-usage`，让服务器用量并入同一份统计。

mod daemon;
mod hosts;
mod report;
mod sync;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "ccr",
    version,
    about = "把服务器上 agent 的用量同步进 CC Switch（自动发现 SSH 主机 + 交互选择）",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出可同步的服务器（自动发现 SSH config / CC Switch Fleet / 手动配置）
    Hosts {
        /// 实际探测连通性与 agent 日志概况
        #[arg(long)]
        probe: bool,
        /// 输出 JSON
        #[arg(long)]
        json: bool,
        /// 无视退避冷却，强制探测全部候选主机
        #[arg(long)]
        force: bool,
    },

    /// 同步服务器会话日志并导入 CC Switch 用量统计
    Sync {
        /// 指定服务器（省略则交互选择）
        hosts: Vec<String>,
        /// 同步全部可达且有日志的服务器
        #[arg(long)]
        all: bool,
        /// 首次回溯天数（默认取配置，2 天）
        #[arg(long)]
        days: Option<u64>,
        /// 忽略断点，强制全量重拉（同时无视退避冷却）
        #[arg(long)]
        force: bool,
        /// 只无视退避冷却，仍走增量同步
        #[arg(long)]
        retry_now: bool,
        /// 只显示计划，不执行
        #[arg(long)]
        dry_run: bool,
        /// 精简输出（给定时任务用）
        #[arg(long)]
        quiet: bool,
    },

    /// 按服务器 / 模型汇总用量（Token 与折算花费）
    Report {
        /// 统计窗口（天）
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// 只看某台服务器
        #[arg(long)]
        host: Option<String>,
        /// 显示逐日明细
        #[arg(long)]
        daily: bool,
        /// 输出 JSON
        #[arg(long)]
        json: bool,
    },

    /// 查看各服务器上次同步状态与镜像占用
    Status,

    /// 管理 launchd 定时任务
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },

    /// 查看 / 修改配置
    Config {
        /// 设置默认同步主机，逗号分隔（传空字符串清空）
        #[arg(long)]
        set_hosts: Option<String>,
        /// 设置默认回溯天数
        #[arg(long)]
        set_days: Option<u64>,
        /// 设置退避：基础分钟,上限分钟（如 15,360）
        #[arg(long)]
        set_backoff: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum DaemonCmd {
    /// 安装/更新定时同步任务（默认每 15 分钟）
    Install {
        #[arg(long, default_value_t = 900)]
        interval: u64,
        /// 只同步这些主机，逗号分隔（默认用配置）
        #[arg(long)]
        hosts: Option<String>,
    },
    /// 卸载定时任务
    Uninstall,
    /// 查看定时任务状态
    Status,
}

// ---------------------------------------------------------------- 配置与状态

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default = "default_days")]
    pub days: u64,
    /// cc-switch 可执行文件路径，留空自动探测
    #[serde(default)]
    pub cc_switch: Option<String>,
    /// 连续失败后的基础冷却（分钟），默认 15
    #[serde(default = "default_backoff_base")]
    pub backoff_base_min: u64,
    /// 冷却上限（分钟），默认 360
    #[serde(default = "default_backoff_max")]
    pub backoff_max_min: u64,
}

fn default_days() -> u64 {
    2
}

fn default_backoff_base() -> u64 {
    15
}

fn default_backoff_max() -> u64 {
    360
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            days: default_days(),
            cc_switch: None,
            backoff_base_min: default_backoff_base(),
            backoff_max_min: default_backoff_max(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct State {
    #[serde(default)]
    pub hosts: BTreeMap<String, HostState>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct HostState {
    /// 上次成功同步的 epoch 秒，作为下次增量起点
    #[serde(default)]
    pub last_epoch: i64,
    /// 上次成功同步时间（可读）
    #[serde(default)]
    pub last_run: String,
    /// 上次导入的用量条数
    #[serde(default)]
    pub last_imported: i64,
    /// 上次扫描的日志文件数
    #[serde(default)]
    pub last_scanned: i64,
    /// 连续失败次数
    #[serde(default)]
    pub failures: u32,
    /// 冷却结束时间（epoch 秒），0 表示无冷却
    #[serde(default)]
    pub next_attempt: i64,
    /// 最近一次失败原因
    #[serde(default)]
    pub last_error: String,
}

/// 指数退避：失败越多、间隔越长，成功后清零。
/// 默认 15m → 30m → 1h → 2h → 4h → 6h（上限），上限可通过配置调整。
fn backoff_secs(cfg: &Config, failures: u32) -> i64 {
    let base = cfg.backoff_base_min.max(1) as i64 * 60;
    let max = cfg
        .backoff_max_min
        .max(cfg.backoff_base_min)
        .max(1) as i64
        * 60;
    let shift = failures.saturating_sub(1).min(10);
    base.saturating_mul(1i64 << shift).min(max)
}

fn now_epoch() -> i64 {
    chrono::Local::now().timestamp()
}

fn fmt_epoch(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

/// 该主机是否还在冷却窗口内；是则返回冷却结束时间。
fn cooling_until(state: &State, host: &str) -> Option<i64> {
    state.hosts.get(host).and_then(|s| {
        if s.next_attempt > now_epoch() {
            Some(s.next_attempt)
        } else {
            None
        }
    })
}

fn record_success(state: &mut State, host: &str, started: i64, imported: i64, scanned: i64) {
    state.hosts.insert(
        host.to_string(),
        HostState {
            last_epoch: started,
            last_run: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            last_imported: imported,
            last_scanned: scanned,
            failures: 0,
            next_attempt: 0,
            last_error: String::new(),
        },
    );
}

fn record_failure(cfg: &Config, state: &mut State, host: &str, err: &str) {
    let prev = state.hosts.get(host).cloned().unwrap_or_default();
    let failures = prev.failures.saturating_add(1);
    let next = now_epoch() + backoff_secs(cfg, failures);
    let brief: String = err.lines().next().unwrap_or("").chars().take(120).collect();
    state.hosts.insert(
        host.to_string(),
        HostState {
            failures,
            next_attempt: next,
            last_error: brief,
            ..prev
        },
    );
}

pub fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 镜像与状态根目录，可用 REMOTE_USAGE_ROOT 覆盖
pub fn root() -> PathBuf {
    if let Ok(r) = std::env::var("REMOTE_USAGE_ROOT") {
        if !r.is_empty() {
            return PathBuf::from(r);
        }
    }
    home().join(".cc-switch").join("remote-usage")
}

fn config_path() -> PathBuf {
    root().join("config.json")
}

fn state_path() -> PathBuf {
    root().join("state.json")
}

pub fn load_config() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_config(cfg: &Config) -> Result<()> {
    fs::create_dir_all(root())?;
    fs::write(config_path(), serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

pub fn load_state() -> State {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_state(s: &State) -> Result<()> {
    fs::create_dir_all(root())?;
    fs::write(state_path(), serde_json::to_string_pretty(s)?)?;
    Ok(())
}

/// 解析 cc-switch 可执行文件：配置 > PATH > 常见 Homebrew 路径
pub fn cc_switch_bin() -> String {
    if let Some(p) = load_config().cc_switch {
        if !p.is_empty() {
            return p;
        }
    }
    if let Some(p) = which("cc-switch") {
        return p;
    }
    "/opt/homebrew/bin/cc-switch".to_string()
}

pub fn which(bin: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let p = PathBuf::from(dir).join(bin);
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

// ---------------------------------------------------------------- 入口

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("错误: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Hosts { probe, json, force } => cmd_hosts(probe, json, force),
        Cmd::Sync {
            hosts,
            all,
            days,
            force,
            retry_now,
            dry_run,
            quiet,
        } => cmd_sync(hosts, all, days, force, retry_now, dry_run, quiet),
        Cmd::Report {
            days,
            host,
            daily,
            json,
        } => report::run(days, host.as_deref(), daily, json),
        Cmd::Status => cmd_status(),
        Cmd::Daemon { action } => match action {
            DaemonCmd::Install { interval, hosts } => {
                let list = hosts
                    .map(|s| {
                        s.split(',')
                            .map(|x| x.trim().to_string())
                            .filter(|x| !x.is_empty())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|| load_config().hosts);
                daemon::install(interval, &list)
            }
            DaemonCmd::Uninstall => daemon::uninstall(),
            DaemonCmd::Status => daemon::status(),
        },
        Cmd::Config {
            set_hosts,
            set_days,
            set_backoff,
        } => cmd_config(set_hosts, set_days, set_backoff),
    }
}

// ---------------------------------------------------------------- hosts

fn cmd_hosts(probe: bool, json: bool, force: bool) -> Result<()> {
    let specs = hosts::discover();
    if specs.is_empty() {
        println!("没有发现任何 SSH 主机（检查 ~/.ssh/config 或用 `ccr config --set-hosts` 指定）");
        return Ok(());
    }

    if !probe {
        if json {
            let arr: Vec<_> = specs
                .iter()
                .map(|s| serde_json::json!({"host": s.id, "source": s.source}))
                .collect();
            println!("{}", serde_json::to_string_pretty(&arr)?);
            return Ok(());
        }
        println!("发现 {} 台候选主机：\n", specs.len());
        println!("  {:<24}{}", "主机", "来源");
        for s in &specs {
            println!("  {:<24}{}", s.id, s.source);
        }
        println!("\n加 --probe 探测连通性与 agent 日志概况。");
        return Ok(());
    }

    let cfg = load_config();
    let mut state = load_state();

    // 冷却中的主机不重复探测，避免反复赔超时
    let mut skipped: Vec<(String, i64)> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for s in &specs {
        if !force {
            if let Some(until) = cooling_until(&state, &s.id) {
                skipped.push((s.id.clone(), until));
                continue;
            }
        }
        ids.push(s.id.clone());
    }

    let results = hosts::probe_many(&ids);
    for (h, p) in &results {
        if !p.ok {
            record_failure(&cfg, &mut state, h, p.err.as_deref().unwrap_or("ssh 失败"));
        }
    }
    let _ = save_state(&state);

    let by_id: BTreeMap<&str, &hosts::Probe> =
        results.iter().map(|(h, p)| (h.as_str(), p)).collect();

    if json {
        let arr: Vec<_> = specs
            .iter()
            .map(|s| {
                let p = by_id.get(s.id.as_str());
                let cooling = skipped
                    .iter()
                    .find(|(h, _)| h == &s.id)
                    .map(|(_, t)| *t);
                serde_json::json!({
                    "host": s.id,
                    "source": s.source,
                    "reachable": p.map(|p| p.ok).unwrap_or(false),
                    "codex_recent_files": p.map(|p| p.codex).unwrap_or(0),
                    "claude_recent_files": p.map(|p| p.claude).unwrap_or(0),
                    "codex_size": p.map(|p| p.size.clone()).unwrap_or_default(),
                    "error": p.and_then(|p| p.err.clone()),
                    "cooling_until": cooling,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    println!(
        "  {:<22}{:<10}{:>8}{:>8}{:>8}  {}",
        "主机", "状态", "Codex", "Claude", "大小", "来源"
    );
    for s in &specs {
        if let Some((_, until)) = skipped.iter().find(|(h, _)| h == &s.id) {
            let failures = state
                .hosts
                .get(&s.id)
                .map(|x| x.failures)
                .unwrap_or(0);
            println!(
                "  {:<22}{:<10}{:>8}{:>8}{:>8}  {}",
                s.id,
                "冷却中",
                0,
                0,
                "-",
                format!("连续失败 {failures} 次，{} 再试", fmt_epoch(*until))
            );
            continue;
        }
        match by_id.get(s.id.as_str()) {
            Some(p) if p.ok => {
                println!(
                    "  {:<22}{:<10}{:>8}{:>8}{:>8}  {}",
                    s.id,
                    if p.active() { "可同步" } else { "无日志" },
                    p.codex,
                    p.claude,
                    if p.size.is_empty() { "-" } else { &p.size },
                    s.source
                );
            }
            Some(p) => {
                println!(
                    "  {:<22}{:<10}{:>8}{:>8}{:>8}  {}",
                    s.id,
                    "不可达",
                    0,
                    0,
                    "-",
                    p.err
                        .as_deref()
                        .unwrap_or("ssh 失败")
                        .chars()
                        .take(40)
                        .collect::<String>()
                );
            }
            None => {}
        }
    }
    if !skipped.is_empty() {
        println!(
            "\n{} 台在退避冷却中（下次重试时间见上表），强制探测加 --force。",
            skipped.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- sync

/// 探测候选主机：跳过冷却中的（避免反复赔超时），把探测失败计入退避。
fn probe_candidates(
    cfg: &Config,
    state: &mut State,
    quiet: bool,
    respect_backoff: bool,
) -> Result<Vec<(String, hosts::Probe)>> {
    let specs = hosts::discover();
    if specs.is_empty() {
        anyhow::bail!("没发现任何 SSH 主机，用 `ccr config --set-hosts a,b` 指定");
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
        if cooling > 0 {
            anyhow::bail!("候选主机都在退避冷却中（{cooling} 台），用 --force 强制重试");
        }
        anyhow::bail!("没有候选主机");
    }

    if !quiet {
        eprintln!("正在探测 {} 台候选主机…", ids.len());
    }
    let results = hosts::probe_many(&ids);
    for (h, p) in &results {
        if !p.ok {
            record_failure(cfg, state, h, p.err.as_deref().unwrap_or("ssh 失败"));
        }
    }
    Ok(results
        .into_iter()
        .filter(|(_, p)| p.active())
        .collect())
}

fn human_dur(secs: i64) -> String {
    let m = (secs / 60).max(1);
    if m < 60 {
        format!("{m} 分钟")
    } else if m % 60 == 0 {
        format!("{} 小时", m / 60)
    } else {
        format!("{} 小时 {} 分钟", m / 60, m % 60)
    }
}

fn cmd_sync(
    mut hosts: Vec<String>,
    all: bool,
    days: Option<u64>,
    force: bool,
    retry_now: bool,
    dry_run: bool,
    quiet: bool,
) -> Result<()> {
    let cfg = load_config();
    let days = days.unwrap_or(cfg.days).max(1);
    let mut state = load_state();

    // 0) 环境变量覆盖（给 launchd 定时任务用）
    if hosts.is_empty() {
        if let Ok(list) = std::env::var("REMOTE_USAGE_HOSTS") {
            hosts = list
                .split_whitespace()
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    // 命令行点名的主机：无视冷却（用户明确要求重试）
    let explicit = !hosts.is_empty();

    // 1) 决定要同步哪些主机
    if hosts.is_empty() && !all {
        if !cfg.hosts.is_empty() {
            hosts = cfg.hosts.clone();
            if !quiet {
                println!("使用配置里的默认主机：{}", hosts.join(", "));
            }
        } else {
            // 自动发现 + 探测 + 交互多选
            let active = probe_candidates(&cfg, &mut state, quiet, !(force || retry_now))?;
            if active.is_empty() {
                anyhow::bail!("没有可达且带 agent 日志的主机");
            }

            let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            if interactive {
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
                let defaults: Vec<bool> = active.iter().map(|_| true).collect();
                let picked = dialoguer::MultiSelect::new()
                    .with_prompt("空格选择要同步的服务器，回车确认")
                    .items(&labels)
                    .defaults(&defaults)
                    .interact()?;
                hosts = picked.into_iter().map(|i| active[i].0.clone()).collect();
                if hosts.is_empty() {
                    println!("没有选择任何服务器，已取消。");
                    return Ok(());
                }
            } else {
                hosts = active.iter().map(|(h, _)| h.clone()).collect();
            }
        }
    } else if hosts.is_empty() {
        // --all：只挑可达且有日志的，避免挨个卡在 SSH 超时上
        let active = probe_candidates(&cfg, &mut state, quiet, !(force || retry_now))?;
        if active.is_empty() {
            anyhow::bail!("没有可达且带 agent 日志的主机");
        }
        hosts = active.iter().map(|(h, _)| h.clone()).collect();
        if !quiet {
            println!("同步 {} 台有日志的主机：{}", hosts.len(), hosts.join(", "));
        }
    }

    if hosts.is_empty() {
        anyhow::bail!("没有指定要同步的主机");
    }

    // 1.5) 退避冷却：连续失败的机器降低尝试频率（命令行点名 / --force 不挡）
    if !force && !retry_now && !explicit {
        let mut cooled: Vec<(String, i64, u32)> = Vec::new();
        let mut ready: Vec<String> = Vec::new();
        for h in &hosts {
            match cooling_until(&state, h) {
                Some(t) => cooled.push((
                    h.clone(),
                    t,
                    state.hosts.get(h).map(|s| s.failures).unwrap_or(0),
                )),
                None => ready.push(h.clone()),
            }
        }
        if !cooled.is_empty() {
            if !quiet {
                println!("跳过 {} 台冷却中的主机：", cooled.len());
                for (h, t, f) in &cooled {
                    println!("  - {h}（连续失败 {f} 次，{} 再试）", fmt_epoch(*t));
                }
            }
            hosts = ready;
        }
        if hosts.is_empty() {
            save_state(&state)?;
            if !quiet {
                println!("本轮没有需要同步的主机（全部在冷却中）。用 --force 强制重试。");
            }
            return Ok(());
        }
    }

    // 2) 逐台同步
    let mut outcomes = Vec::new();
    let total = hosts.len();

    for (i, host) in hosts.iter().enumerate() {
        if !quiet {
            eprintln!("[{}/{}] {}", i + 1, total, host);
        }
        if dry_run {
            println!("  (dry-run) 将同步 {host}，回溯 {days} 天");
            continue;
        }
        let since = if force {
            0
        } else {
            state
                .hosts
                .get(host)
                .map(|h| h.last_epoch)
                .filter(|e| *e > 0)
                .unwrap_or_else(|| chrono::Local::now().timestamp() - (days as i64) * 86400)
        };
        let started = chrono::Local::now().timestamp();
        let outcome = sync::sync_host(host, since, quiet);
        if !quiet {
            match &outcome.err {
                None => println!(
                    "  ✓ 导入 {} 条（扫描 {} 个文件，镜像 {} 个）",
                    outcome.imported, outcome.scanned, outcome.mirror_files
                ),
                Some(e) => {
                    println!("  ✗ {e}");
                    let next_failures =
                        state.hosts.get(host).map(|s| s.failures).unwrap_or(0) + 1;
                    println!(
                        "    已降低重试频率：{} 后再试（连续失败 {} 次）",
                        human_dur(backoff_secs(&cfg, next_failures)),
                        next_failures
                    );
                }
            }
        }
        match &outcome.err {
            None => record_success(
                &mut state,
                host,
                started,
                outcome.imported,
                outcome.scanned,
            ),
            Some(e) => record_failure(&cfg, &mut state, host, e),
        }
        outcomes.push(outcome);
    }

    if dry_run {
        println!("\n(dry-run) 未执行任何同步。");
        return Ok(());
    }

    save_state(&state)?;

    if !quiet {
        let ok = outcomes.iter().filter(|o| o.err.is_none()).count();
        let imported: i64 = outcomes.iter().map(|o| o.imported).sum();
        println!(
            "\n完成：{ok}/{} 台成功，共导入 {imported} 条用量记录（CC Switch 看板约 30 秒内刷新）",
            outcomes.len()
        );
        if imported > 0 {
            println!("查看明细：ccr report --days 2");
        }
    }

    let failed: Vec<_> = outcomes.iter().filter(|o| o.err.is_some()).collect();
    if !failed.is_empty() && quiet {
        for o in failed {
            eprintln!("{}: {}", o.host, o.err.clone().unwrap_or_default());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- status

fn cmd_status() -> Result<()> {
    let state = load_state();
    if state.hosts.is_empty() {
        println!("还没有同步记录。先跑一次：ccr sync");
        return Ok(());
    }
    println!(
        "  {:<20}{:<21}{:>8}{:>6}{:>16}{:>10}",
        "主机", "上次成功同步", "导入", "失败", "下次尝试", "镜像"
    );
    for (host, hs) in &state.hosts {
        let dir = root().join(host);
        let bytes = report::dir_size(&dir);
        let next = if hs.next_attempt > now_epoch() {
            fmt_epoch(hs.next_attempt)
        } else {
            "-".to_string()
        };
        println!(
            "  {:<20}{:<21}{:>8}{:>6}{:>16}{:>10}",
            host,
            if hs.last_run.is_empty() {
                "-"
            } else {
                &hs.last_run
            },
            hs.last_imported,
            if hs.failures > 0 {
                hs.failures.to_string()
            } else {
                "-".to_string()
            },
            next,
            report::human_size(bytes)
        );
    }

    let failing: Vec<_> = state
        .hosts
        .iter()
        .filter(|(_, s)| s.failures > 0 && !s.last_error.is_empty())
        .collect();
    if !failing.is_empty() {
        println!("\n失败原因：");
        for (h, s) in failing {
            let backoff = human_dur(backoff_secs(&load_config(), s.failures));
            println!(
                "  {h:<20}连续失败 {} 次（退避 {backoff}）：{}",
                s.failures, s.last_error
            );
        }
        println!("\n想立刻重试：ccr sync <主机>  或  ccr sync --all --force");
    }

    println!("\n根目录：{}", root().display());
    Ok(())
}

// ---------------------------------------------------------------- config

fn cmd_config(
    set_hosts: Option<String>,
    set_days: Option<u64>,
    set_backoff: Option<String>,
) -> Result<()> {
    let mut cfg = load_config();
    let mut changed = false;

    if let Some(h) = set_hosts {
        cfg.hosts = h
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
        changed = true;
        println!("默认主机：{:?}", cfg.hosts);
    }
    if let Some(d) = set_days {
        cfg.days = d.max(1);
        changed = true;
        println!("默认回溯天数：{}", cfg.days);
    }
    if let Some(b) = set_backoff {
        let parts: Vec<&str> = b.split(',').map(|x| x.trim()).collect();
        if parts.len() != 2 {
            anyhow::bail!("格式应为 基础分钟,上限分钟，例如 15,360");
        }
        let base: u64 = parts[0].parse()?;
        let max: u64 = parts[1].parse()?;
        if base == 0 || max < base {
            anyhow::bail!("要求 基础 > 0 且 上限 >= 基础");
        }
        cfg.backoff_base_min = base;
        cfg.backoff_max_min = max;
        changed = true;
        println!("退避：基础 {base} 分钟，上限 {max} 分钟");
    }
    if changed {
        save_config(&cfg)?;
    }

    println!("\n配置文件：{}", config_path().display());
    println!("  hosts   = {:?}", cfg.hosts);
    println!("  days    = {}", cfg.days);
    println!("  cc_switch = {:?}", cfg.cc_switch);
    println!(
        "  backoff = 基础 {} 分钟 → 上限 {} 分钟",
        cfg.backoff_base_min, cfg.backoff_max_min
    );
    println!("  cc-switch 实际路径 = {}", cc_switch_bin());
    println!("  镜像根目录 = {}", root().display());
    let mut out = std::io::stdout();
    let _ = out.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let cfg = Config::default(); // 基础 15 分钟，上限 360 分钟
        assert_eq!(backoff_secs(&cfg, 1), 15 * 60);
        assert_eq!(backoff_secs(&cfg, 2), 30 * 60);
        assert_eq!(backoff_secs(&cfg, 3), 60 * 60);
        assert_eq!(backoff_secs(&cfg, 4), 120 * 60);
        assert_eq!(backoff_secs(&cfg, 5), 240 * 60);
        assert_eq!(backoff_secs(&cfg, 6), 360 * 60);
        assert_eq!(backoff_secs(&cfg, 60), 360 * 60, "不能超过上限");
    }

    #[test]
    fn backoff_respects_config() {
        let mut cfg = Config::default();
        cfg.backoff_base_min = 1;
        cfg.backoff_max_min = 5;
        assert_eq!(backoff_secs(&cfg, 1), 60);
        assert_eq!(backoff_secs(&cfg, 4), 5 * 60);
    }

    #[test]
    fn human_dur_formats() {
        assert_eq!(human_dur(60), "1 分钟");
        assert_eq!(human_dur(900), "15 分钟");
        assert_eq!(human_dur(3600), "1 小时");
        assert_eq!(human_dur(5400), "1 小时 30 分钟");
    }

    #[test]
    fn cooling_window() {
        let mut st = State::default();
        st.hosts.insert(
            "hot".into(),
            HostState {
                next_attempt: now_epoch() + 600,
                ..Default::default()
            },
        );
        st.hosts.insert(
            "expired".into(),
            HostState {
                next_attempt: now_epoch() - 600,
                ..Default::default()
            },
        );
        assert!(cooling_until(&st, "hot").is_some());
        assert!(cooling_until(&st, "expired").is_none());
        assert!(cooling_until(&st, "never-seen").is_none());
    }

    #[test]
    fn failure_accumulates_and_success_resets() {
        let cfg = Config::default();
        let mut st = State::default();

        record_failure(&cfg, &mut st, "h", "Connection timed out\nsecond line");
        assert_eq!(st.hosts["h"].failures, 1);
        assert_eq!(st.hosts["h"].last_error, "Connection timed out");
        assert!(st.hosts["h"].next_attempt > now_epoch());

        record_failure(&cfg, &mut st, "h", "again");
        assert_eq!(st.hosts["h"].failures, 2);

        record_success(&mut st, "h", 12345, 7, 9);
        assert_eq!(st.hosts["h"].failures, 0);
        assert_eq!(st.hosts["h"].next_attempt, 0);
        assert_eq!(st.hosts["h"].last_epoch, 12345);
        assert_eq!(st.hosts["h"].last_imported, 7);
        assert!(st.hosts["h"].last_error.is_empty());
        assert!(cooling_until(&st, "h").is_none());
    }
}
