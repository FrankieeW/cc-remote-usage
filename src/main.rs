//! ccr — 把服务器上 Codex / Claude Code agent 的用量同步进 CC Switch。
//!
//! CC Switch 的「用量」看板只扫本机的会话日志（`~/.codex/sessions`、
//! `~/.claude/projects`，受 `CODEX_HOME` / `CLAUDE_CONFIG_DIR` 控制）。
//! 本工具把各服务器上的会话日志增量拉回本机镜像目录，再用作用域环境变量
//! 调 `cc-switch sessions sync-usage`，让服务器用量并入同一份统计。

mod daemon;
mod extra_sources;
mod hosts;
mod import_cmd;
mod importer;
mod mirror;
mod report;
mod select;
mod ssh;
mod state;
mod sync;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use select::{
    check_hosts, initial_hosts, parse_host_list, pick_hosts, probe_candidates, skip_cooling,
};
use state::{
    backoff_secs, cc_switch_bin, config_path, cooling_until, load_config, load_state, now_epoch,
    record_failure, record_success, root, save_config, save_state, Config, State,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::time::SystemTime;

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

    /// 只重试本机已有镜像的用量导入，不连接服务器
    Import {
        #[arg(required = true)]
        hosts: Vec<String>,
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
    Config(ConfigArgs),
}

#[derive(clap::Args, Debug)]
struct ConfigArgs {
    /// 设置默认同步主机，逗号分隔（传空字符串清空）
    #[arg(long)]
    set_hosts: Option<String>,
    /// 设置默认回溯天数
    #[arg(long)]
    set_days: Option<u64>,
    /// 设置退避：基础分钟,上限分钟（如 15,360）
    #[arg(long)]
    set_backoff: Option<String>,
    /// 设置镜像保留天数（按文件修改时间清理，0 表示不清理）
    #[arg(long)]
    set_retain_days: Option<u64>,
    /// 将当前安装的 cc-switch 版本锁定为已确认的兼容版本
    #[arg(long, conflicts_with = "unpin_cc_switch")]
    pin_cc_switch: bool,
    /// 取消 cc-switch 版本锁定
    #[arg(long)]
    unpin_cc_switch: bool,
    /// 添加额外 Codex 日志根目录，格式为 主机=远端绝对路径；可重复指定
    #[arg(long)]
    add_codex_root: Vec<String>,
    /// 清空指定主机的额外 Codex 日志目录
    #[arg(long)]
    clear_codex_roots: Option<String>,
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
        Cmd::Import { hosts, quiet } => import_cmd::run(hosts, quiet),
        Cmd::Report {
            days,
            host,
            daily,
            json,
        } => report::run(
            days,
            host.as_deref(),
            daily,
            json,
            load_config().retain_days,
        ),
        Cmd::Status => cmd_status(),
        Cmd::Daemon { action } => match action {
            DaemonCmd::Install { interval, hosts } => {
                let list = match hosts {
                    Some(s) => parse_host_list(&s)?,
                    None => {
                        let configured = load_config().hosts;
                        check_hosts(&configured)?;
                        configured
                    }
                };
                daemon::install(interval, &list)
            }
            DaemonCmd::Uninstall => daemon::uninstall(),
            DaemonCmd::Status => daemon::status(),
        },
        Cmd::Config(args) => cmd_config(args),
    }
}

// ---------------------------------------------------------------- 公共

fn fmt_epoch(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
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
        println!("  {:<24}来源", "主机");
        for s in &specs {
            println!("  {:<24}{}", s.id, s.source);
        }
        println!("\n加 --probe 探测连通性与 agent 日志概况。");
        return Ok(());
    }

    let cfg = load_config();
    let state = load_state();

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

    let (results, local_fps) = hosts::probe_with_local(&ids);
    // 探测期间不占锁（免得挡住定时任务）；落盘时再拿锁，基于最新状态只合并失败记录，
    // 不会用探测开始时读到的旧状态覆盖同步刚写下的断点
    let recorded = match state::try_lock_runs()? {
        Some(_lock) => {
            let mut fresh = load_state();
            for (h, p) in &results {
                if !p.ok {
                    record_failure(&cfg, &mut fresh, h, p.err.as_deref().unwrap_or("ssh 失败"));
                }
            }
            let _ = save_state(&fresh);
            true
        }
        None => false,
    };

    let mut dups = hosts::duplicates(&results, &local_fps, |h| hosts::source_rank(&specs, h));
    dups.retain(|host, _| !cfg.has_extra_codex_roots(host));
    let by_id: BTreeMap<&str, &hosts::Probe> =
        results.iter().map(|(h, p)| (h.as_str(), p)).collect();

    if json {
        let arr: Vec<_> = specs
            .iter()
            .map(|s| {
                let p = by_id.get(s.id.as_str());
                let cooling = skipped.iter().find(|(h, _)| h == &s.id).map(|(_, t)| *t);
                let dup = dups.get(&s.id);
                serde_json::json!({
                    "host": s.id,
                    "source": s.source,
                    "reachable": p.map(|p| p.ok).unwrap_or(false),
                    "codex_recent_files": p.map(|p| p.codex).unwrap_or(0),
                    "claude_recent_files": p.map(|p| p.claude).unwrap_or(0),
                    "codex_size": p.map(|p| p.size.clone()).unwrap_or_default(),
                    "extra_codex_roots": cfg.extra_codex_roots.get(&s.id),
                    "error": p.and_then(|p| p.err.clone()),
                    "cooling_until": cooling,
                    "duplicate_of": match dup {
                        Some(hosts::Skip::SameAs(h)) => Some(h.as_str()),
                        _ => None,
                    },
                    "is_local": matches!(dup, Some(hosts::Skip::Local)),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    println!(
        "  {:<22}{:<10}{:>8}{:>8}{:>8}  来源",
        "主机", "状态", "Codex", "Claude", "大小"
    );
    for s in &specs {
        if let Some((_, until)) = skipped.iter().find(|(h, _)| h == &s.id) {
            let failures = state.hosts.get(&s.id).map(|x| x.failures).unwrap_or(0);
            println!(
                "  {:<22}{:<10}{:>8}{:>8}{:>8}  连续失败 {failures} 次，{} 再试",
                s.id,
                "冷却中",
                0,
                0,
                "-",
                fmt_epoch(*until)
            );
            continue;
        }
        let Some(p) = by_id.get(s.id.as_str()) else {
            continue;
        };
        let size = if p.size.is_empty() { "-" } else { &p.size };
        if !p.ok {
            let err: String = p
                .err
                .as_deref()
                .unwrap_or("ssh 失败")
                .chars()
                .take(40)
                .collect();
            println!(
                "  {:<22}{:<10}{:>8}{:>8}{:>8}  {err}",
                s.id, "不可达", 0, 0, "-"
            );
            continue;
        }
        let (status, note) = match dups.get(&s.id) {
            Some(hosts::Skip::Local) => ("本机", hosts::Skip::Local.describe()),
            Some(skip) => ("重复", skip.describe()),
            None if cfg.has_extra_codex_roots(&s.id) => {
                ("可同步", "已配置额外 Codex 日志目录".into())
            }
            None if p.active() => ("可同步", s.source.clone()),
            None => ("无日志", s.source.clone()),
        };
        println!(
            "  {:<22}{:<10}{:>8}{:>8}{:>8}  {note}",
            s.id, status, p.codex, p.claude, size
        );
    }
    if !skipped.is_empty() {
        println!(
            "\n{} 台在退避冷却中（下次重试时间见上表），强制探测加 --force。",
            skipped.len()
        );
    }
    if !dups.is_empty() {
        println!("\n标为重复 / 本机的主机在 sync --all 时自动跳过（同一份日志只拉一次）。");
    }
    if !recorded {
        println!("\n（有同步正在运行，本次探测结果没有计入退避状态）");
    }
    Ok(())
}

// ---------------------------------------------------------------- sync

/// 导入完成后按保留期清理镜像（先导入后清理，删掉的都是早已入库的旧文件）。
/// 配置文件解析失败时不清理：按默认值删文件可能违背用户的设置（比如 `retain_days: 0`）。
fn prune_mirrors(cfg: &Config, config_ok: bool, quiet: bool) {
    if !config_ok {
        if !quiet {
            println!("配置文件解析失败，本轮不清理镜像。");
        }
        return;
    }
    let pruned = mirror::prune(&root(), cfg.retain_days, SystemTime::now());
    if !quiet && pruned.files > 0 {
        println!(
            "清理镜像：删除 {} 个超过 {} 天未更新的日志，释放 {}",
            pruned.files,
            cfg.retain_days,
            report::human_size(pruned.bytes)
        );
    }
}

fn print_outcome(cfg: &Config, state: &State, outcome: &sync::Outcome) {
    match &outcome.err {
        None => println!(
            "  ✓ 导入 {} 条（扫描 {} 个文件，镜像 {} 个）",
            outcome.imported, outcome.scanned, outcome.mirror_files
        ),
        Some(e) => {
            let failures = state
                .hosts
                .get(&outcome.host)
                .map(|s| s.failures)
                .unwrap_or(1);
            println!("  ✗ {e}");
            println!(
                "    已降低重试频率：{} 后再试（连续失败 {failures} 次）",
                human_dur(backoff_secs(cfg, failures))
            );
        }
    }
    for line in sync::warning_summary(&outcome.warnings) {
        println!("  ⚠ cc-switch：{line}");
    }
}

fn cmd_sync(
    cli_hosts: Vec<String>,
    all: bool,
    days: Option<u64>,
    force: bool,
    retry_now: bool,
    dry_run: bool,
    quiet: bool,
) -> Result<()> {
    let cfg = state::read_config()?;
    extra_sources::validate_config(&cfg)?;
    let days = days.unwrap_or(cfg.days).max(1);
    let (mut hosts, explicit) = initial_hosts(cli_hosts, std::env::var("REMOTE_USAGE_HOSTS").ok());
    let respect_backoff = !(force || retry_now);

    // 0) 同一时刻只跑一个同步（定时任务与手动执行会撞车）；dry-run 不改任何东西，不必抢锁
    let mut lock = None;
    if !dry_run {
        lock = state::try_lock_runs()?;
        if lock.is_none() {
            if !quiet {
                println!("另一个 ccr sync 正在运行（可能是定时任务），本次跳过。");
            }
            return Ok(());
        }
    }
    // 拿到锁之后再读状态，才能看到上一个进程写下的断点
    let mut state = load_state();
    let importer = importer::Importer::check(&cfg)?;

    // 1) 决定要同步哪些主机
    if hosts.is_empty() {
        if !all && !cfg.hosts.is_empty() {
            hosts = cfg.hosts.clone();
            if !quiet {
                println!("使用配置里的默认主机：{}", hosts.join(", "));
            }
        } else {
            // 自动发现 + 探测：只挑可达且有日志的，避免挨个卡在 SSH 超时上
            let probed = probe_candidates(&cfg, &mut state, quiet, respect_backoff)?;
            if !dry_run {
                // 探测失败也要计入退避，哪怕下面一台可同步的都没有
                save_state(&state)?;
            }
            if probed.active.is_empty() {
                if !dry_run {
                    prune_mirrors(&cfg, true, quiet);
                }
                if probed.all_unreachable() {
                    // 全军覆没多半是本机网络或 SSH agent 的问题，必须让定时任务日志看得到
                    let (h, e) = &probed.unreachable[0];
                    bail!(
                        "本轮探测的 {} 台主机全部连不上（如 {h}：{e}），\
                         先检查本机网络 / SSH agent；各台原因见 ccr status",
                        probed.probed
                    );
                }
                if !quiet {
                    println!("没有可达且带近期 agent 日志的主机（详情：ccr hosts --probe）。");
                }
                return Ok(());
            }
            hosts = if all || !select::interactive() {
                probed.active.iter().map(|(h, _)| h.clone()).collect()
            } else {
                // 交互多选可能停很久，期间不占锁，免得定时任务整轮被跳过
                drop(lock.take());
                let picked = pick_hosts(&probed.active)?;
                if !dry_run {
                    lock = state::try_lock_runs()?;
                    if lock.is_none() {
                        println!("选择期间定时任务开始了同步，本次跳过，稍后再试。");
                        return Ok(());
                    }
                    state = load_state();
                }
                picked
            };
            if hosts.is_empty() {
                println!("没有选择任何服务器，已取消。");
                return Ok(());
            }
            if all && !quiet {
                println!("同步 {} 台有日志的主机：{}", hosts.len(), hosts.join(", "));
            }
        }
    }
    // 不管来自命令行、定时任务还是配置文件，主机名都会被当作 ssh 参数和镜像目录名
    check_hosts(&hosts)?;

    // 1.5) 退避冷却：连续失败的机器降低尝试频率（命令行点名 / --force / --retry-now 不挡）
    if respect_backoff && !explicit {
        hosts = skip_cooling(&state, hosts, quiet);
        if hosts.is_empty() {
            if !quiet {
                println!("本轮没有需要同步的主机（全部在冷却中）。立即重试用 --retry-now。");
            }
            if !dry_run {
                prune_mirrors(&cfg, true, quiet);
            }
            return Ok(());
        }
    }

    if dry_run {
        for (i, host) in hosts.iter().enumerate() {
            eprintln!("[{}/{}] {}", i + 1, hosts.len(), host);
            println!("  (dry-run) 将同步 {host}，回溯 {days} 天");
        }
        println!("\n(dry-run) 未执行任何同步。");
        return Ok(());
    }

    // 2) 逐台同步；每台结束立即落盘，中途被杀也不丢已完成主机的断点
    let mut outcomes = Vec::new();
    for (i, host) in hosts.iter().enumerate() {
        if !quiet {
            eprintln!("[{}/{}] {}", i + 1, hosts.len(), host);
        }
        let since = if force {
            0
        } else {
            state
                .hosts
                .get(host)
                .map(|h| h.last_epoch)
                .filter(|e| *e > 0)
                .unwrap_or_else(|| now_epoch() - (days as i64) * 86400)
        };
        let started = now_epoch();
        let plan = sync::HostSync {
            host,
            since,
            extra_codex_roots: cfg.extra_codex_roots.get(host).map_or(&[], Vec::as_slice),
        };
        let outcome = sync::sync_host(plan, &importer.bin);
        match &outcome.err {
            None => record_success(&mut state, host, started, outcome.imported, outcome.scanned),
            Some(e) => record_failure(&cfg, &mut state, host, e),
        }
        save_state(&state)?;
        if !quiet {
            print_outcome(&cfg, &state, &outcome);
        }
        outcomes.push(outcome);
    }

    // 3) 清理过期镜像（必须在导入之后）
    prune_mirrors(&cfg, true, quiet);

    let ok = outcomes.iter().filter(|o| o.err.is_none()).count();
    let imported: i64 = outcomes.iter().map(|o| o.imported).sum();

    if quiet {
        for o in &outcomes {
            if let Some(e) = &o.err {
                eprintln!("{}: {e}", o.host);
            }
            for line in sync::warning_summary(&o.warnings) {
                eprintln!("{}: cc-switch：{line}", o.host);
            }
        }
    } else {
        println!(
            "\n完成：{ok}/{} 台成功，共导入 {imported} 条用量记录（CC Switch 看板约 30 秒内刷新）",
            outcomes.len()
        );
        if imported > 0 {
            println!("查看明细：ccr report --days 2");
        }
    }
    if ok < outcomes.len() {
        bail!("{} 台同步失败", outcomes.len() - ok);
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
    let cfg = load_config();
    println!(
        "  {:<23}{:<21}{:>8}{:>6}{:>16}{:>10}",
        "主机", "上次成功同步", "导入", "失败", "下次尝试", "镜像"
    );
    for (host, hs) in &state.hosts {
        let bytes = report::dir_size(&root().join(host));
        let next = cooling_until(&state, host)
            .map(fmt_epoch)
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<23}{:<21}{:>8}{:>6}{:>16}{:>10}",
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
            let backoff = human_dur(backoff_secs(&cfg, s.failures));
            println!(
                "  {h:<23}连续失败 {} 次（退避 {backoff}）：{}",
                s.failures, s.last_error
            );
        }
        println!("\n想立刻重试：ccr sync <主机>  或  ccr sync --all --retry-now");
    }

    let retain = match cfg.retain_days {
        0 => "不清理".to_string(),
        d => format!("{d} 天（按文件修改时间）"),
    };
    println!("\n镜像保留：{retain}");
    println!("根目录：{}", root().display());
    Ok(())
}

// ---------------------------------------------------------------- config

fn cmd_config(args: ConfigArgs) -> Result<()> {
    let ConfigArgs {
        set_hosts,
        set_days,
        set_backoff,
        set_retain_days,
        pin_cc_switch,
        unpin_cc_switch,
        add_codex_root,
        clear_codex_roots,
    } = args;
    let changing = set_hosts.is_some()
        || set_days.is_some()
        || set_backoff.is_some()
        || set_retain_days.is_some()
        || pin_cc_switch
        || unpin_cc_switch
        || !add_codex_root.is_empty()
        || clear_codex_roots.is_some();
    // 要改配置时严格读取：文件写坏了就报错，不能拿默认值把它整个覆盖掉
    let mut cfg = if changing {
        state::read_config()?
    } else {
        load_config()
    };

    if let Some(h) = set_hosts {
        cfg.hosts = parse_host_list(&h)?;
        println!("默认主机：{:?}", cfg.hosts);
    }
    if let Some(d) = set_days {
        cfg.days = d.max(1);
        println!("默认回溯天数：{}", cfg.days);
    }
    if let Some(b) = set_backoff {
        let parts: Vec<&str> = b.split(',').map(|x| x.trim()).collect();
        if parts.len() != 2 {
            bail!("格式应为 基础分钟,上限分钟，例如 15,360");
        }
        let base: u64 = parts[0].parse()?;
        let max: u64 = parts[1].parse()?;
        if base == 0 || max < base {
            bail!("要求 基础 > 0 且 上限 >= 基础");
        }
        cfg.backoff_base_min = base;
        cfg.backoff_max_min = max;
        println!("退避：基础 {base} 分钟，上限 {max} 分钟");
    }
    if let Some(r) = set_retain_days {
        cfg.retain_days = r;
        println!("镜像保留天数：{r}（0 表示不清理）");
    }
    if pin_cc_switch {
        let importer = importer::Importer::detect(&cfg)?;
        println!("锁定 cc-switch 版本：{}", importer.version);
        cfg.cc_switch_version = Some(importer.version);
    }
    if unpin_cc_switch {
        cfg.cc_switch_version = None;
    }
    if let Some(host) = clear_codex_roots {
        check_hosts(std::slice::from_ref(&host))?;
        cfg.extra_codex_roots.remove(&host);
    }
    for source in add_codex_root {
        let Some((host, path)) = source.split_once('=') else {
            bail!("额外目录格式应为 主机=/远端/目录");
        };
        check_hosts(&[host.to_owned()])?;
        extra_sources::validate_root(path)?;
        let roots = cfg.extra_codex_roots.entry(host.to_owned()).or_default();
        let path = path.trim_end_matches('/').to_owned();
        if !roots.contains(&path) {
            roots.push(path);
            roots.sort();
        }
    }
    if changing {
        save_config(&cfg)?;
    }

    println!("\n配置文件：{}", config_path().display());
    println!("  hosts   = {:?}", cfg.hosts);
    println!("  days    = {}", cfg.days);
    println!("  cc_switch = {:?}", cfg.cc_switch);
    println!("  cc_switch_version = {:?}", cfg.cc_switch_version);
    println!("  extra_codex_roots = {:?}", cfg.extra_codex_roots);
    println!(
        "  backoff = 基础 {} 分钟 → 上限 {} 分钟",
        cfg.backoff_base_min, cfg.backoff_max_min
    );
    println!("  retain_days = {}", cfg.retain_days);
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
    fn human_dur_formats() {
        assert_eq!(human_dur(60), "1 分钟");
        assert_eq!(human_dur(900), "15 分钟");
        assert_eq!(human_dur(3600), "1 小时");
        assert_eq!(human_dur(5400), "1 小时 30 分钟");
    }
}
