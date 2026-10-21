//! 持久化的配置与同步状态（`config.json` / `state.json`）、失败退避策略、运行锁。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Once;

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
    /// 镜像保留天数（按文件 mtime），0 表示不清理；默认 7，与 `ccr report` 默认窗口一致
    #[serde(default = "default_retain_days")]
    pub retain_days: u64,
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

fn default_retain_days() -> u64 {
    7
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            days: default_days(),
            cc_switch: None,
            backoff_base_min: default_backoff_base(),
            backoff_max_min: default_backoff_max(),
            retain_days: default_retain_days(),
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

// ---------------------------------------------------------------- 退避

/// 指数退避：失败越多、间隔越长，成功后清零。
/// 默认 15m → 30m → 1h → 2h → 4h → 6h（上限），上限可通过配置调整。
pub fn backoff_secs(cfg: &Config, failures: u32) -> i64 {
    let base = cfg.backoff_base_min.max(1) as i64 * 60;
    let max = cfg.backoff_max_min.max(cfg.backoff_base_min).max(1) as i64 * 60;
    let shift = failures.saturating_sub(1).min(10);
    base.saturating_mul(1i64 << shift).min(max)
}

/// 冷却判定的余量：定时任务间隔通常等于基础退避（15 分钟），而失败时刻比本轮开始晚几秒，
/// 不留余量的话首次失败要等两轮（30 分钟）才会重试。
const COOLDOWN_GRACE_SECS: i64 = 60;

pub fn now_epoch() -> i64 {
    chrono::Local::now().timestamp()
}

/// 该主机是否还在冷却窗口内；是则返回冷却结束时间。
pub fn cooling_until(state: &State, host: &str) -> Option<i64> {
    state.hosts.get(host).and_then(|s| {
        if s.next_attempt > now_epoch() + COOLDOWN_GRACE_SECS {
            Some(s.next_attempt)
        } else {
            None
        }
    })
}

pub fn record_success(state: &mut State, host: &str, started: i64, imported: i64, scanned: i64) {
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

pub fn record_failure(cfg: &Config, state: &mut State, host: &str, err: &str) {
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

// ---------------------------------------------------------------- 路径

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

pub fn config_path() -> PathBuf {
    root().join("config.json")
}

fn state_path() -> PathBuf {
    root().join("state.json")
}

// ---------------------------------------------------------------- 读写

/// 严格读取：文件不存在 → 默认值；存在但解析失败 → 报错。
/// 改配置前用它，避免把用户手改写坏的文件整个覆盖成默认值。
pub fn read_config() -> Result<Config> {
    read_json_or_default(&config_path())
}

/// 宽松读取：解析失败时提示一次并按默认配置运行（同步不能因为配置写坏就整体停摆）。
pub fn load_config() -> Config {
    load_config_checked().0
}

/// 同 [`load_config`]，另外告诉调用方配置是否正常解析——
/// 解析失败时不能做删除类操作（比如用户设了 `retain_days: 0`，默认值却会开始清理镜像）。
pub fn load_config_checked() -> (Config, bool) {
    match read_config() {
        Ok(cfg) => (cfg, true),
        Err(e) => {
            static WARN: Once = Once::new();
            WARN.call_once(|| eprintln!("警告: {e:#}，本次按默认配置运行"));
            (Config::default(), false)
        }
    }
}

pub fn save_config(cfg: &Config) -> Result<()> {
    write_atomic(
        &config_path(),
        serde_json::to_string_pretty(cfg)?.as_bytes(),
    )
}

pub fn load_state() -> State {
    read_json_or_default(&state_path()).unwrap_or_else(|e| {
        static WARN: Once = Once::new();
        WARN.call_once(|| eprintln!("警告: {e:#}，增量断点与退避记录将重新开始"));
        State::default()
    })
}

pub fn save_state(s: &State) -> Result<()> {
    write_atomic(&state_path(), serde_json::to_string_pretty(s)?.as_bytes())
}

fn read_json_or_default<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("{} 解析失败", path.display()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("读取 {} 失败", path.display())),
    }
}

/// 先写临时文件再 rename：进程中途被杀也不会留下半截 JSON。
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("目标路径没有父目录")?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().context("目标路径没有文件名")?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let written = fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    let result = written.and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("写入 {} 失败", path.display()))
}

// ---------------------------------------------------------------- 运行锁

/// 同一时刻只允许一个进程改镜像与状态：定时任务与手动 `ccr sync` 可能撞车，
/// 并发解包同一镜像目录、并发写 state.json。拿不到锁返回 None；
/// 锁随返回的 File 一起释放（进程退出时内核也会释放）。
pub fn try_lock_runs() -> Result<Option<fs::File>> {
    try_lock_at(&root().join("sync.lock"))
}

fn try_lock_at(path: &Path) -> Result<Option<fs::File>> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("无法打开锁文件 {}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("无法锁定 {}", path.display()))
        }
    }
}

// ---------------------------------------------------------------- cc-switch

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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 每个测试一个独立临时目录（测试并行运行，不能共用路径，也不能改环境变量）。
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ccr-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 写一个小文件并把 mtime 调到 `age` 之前。
    pub(crate) fn touch(p: &Path, age: std::time::Duration) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"{}\n").unwrap();
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_modified(std::time::SystemTime::now() - age).unwrap();
    }

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
        let cfg = Config {
            backoff_base_min: 1,
            backoff_max_min: 5,
            ..Default::default()
        };
        assert_eq!(backoff_secs(&cfg, 1), 60);
        assert_eq!(backoff_secs(&cfg, 4), 5 * 60);
    }

    fn with_next_attempt(host: &str, next_attempt: i64) -> State {
        let mut st = State::default();
        st.hosts.insert(
            host.into(),
            HostState {
                next_attempt,
                ..Default::default()
            },
        );
        st
    }

    #[test]
    fn cooling_window() {
        let hot = with_next_attempt("hot", now_epoch() + 600);
        let expired = with_next_attempt("expired", now_epoch() - 600);
        assert!(cooling_until(&hot, "hot").is_some());
        assert!(cooling_until(&expired, "expired").is_none());
        assert!(cooling_until(&hot, "never-seen").is_none());
    }

    #[test]
    fn cooling_ends_slightly_early_for_the_next_daemon_tick() {
        // 失败发生在本轮开始后几十秒，15 分钟后的下一轮不应因此再被跳过
        let st = with_next_attempt("h", now_epoch() + 30);
        assert!(cooling_until(&st, "h").is_none());
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

    #[test]
    fn failure_keeps_last_epoch() {
        let cfg = Config::default();
        let mut st = State::default();
        record_success(&mut st, "h", 12345, 1, 1);
        record_failure(&cfg, &mut st, "h", "boom");
        assert_eq!(
            st.hosts["h"].last_epoch, 12345,
            "失败不能推进也不能清掉增量断点"
        );
    }

    #[test]
    fn config_missing_file_is_default_and_old_files_get_new_fields() {
        let d = scratch("config-read");
        let missing: Config = read_json_or_default(&d.join("nope.json")).unwrap();
        assert_eq!(missing.retain_days, 7);

        let old = d.join("old.json");
        fs::write(&old, r#"{"hosts":["netcup"],"days":3}"#).unwrap();
        let cfg: Config = read_json_or_default(&old).unwrap();
        assert_eq!(cfg.hosts, vec!["netcup"]);
        assert_eq!(cfg.days, 3);
        assert_eq!(cfg.retain_days, 7, "旧配置没有 retain_days 时取默认值");
        assert_eq!(cfg.backoff_base_min, 15);
    }

    #[test]
    fn config_parse_error_is_reported_not_swallowed() {
        let d = scratch("config-bad");
        let bad = d.join("config.json");
        fs::write(&bad, r#"{"hosts": ["netcup",]}"#).unwrap();
        let err = read_json_or_default::<Config>(&bad).unwrap_err();
        assert!(format!("{err:#}").contains("解析失败"));
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp_files() {
        let d = scratch("atomic");
        let p = d.join("state.json");
        write_atomic(&p, b"{\"a\":1}").unwrap();
        write_atomic(&p, b"{\"a\":2}").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"a\":2}");
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件应已 rename 掉");
    }

    #[test]
    fn run_lock_is_exclusive_until_dropped() {
        let d = scratch("lock");
        let p = d.join("sync.lock");
        let first = try_lock_at(&p).unwrap();
        assert!(first.is_some());
        assert!(try_lock_at(&p).unwrap().is_none(), "已有人持锁时应拿不到");
        drop(first);
        // 其他测试线程同时在派生子进程：fork 到 exec 之间子进程会短暂持有本进程所有
        // 文件描述符的副本（含这把锁），所以释放后要允许稍等片刻
        let mut again = None;
        for _ in 0..50 {
            again = try_lock_at(&p).unwrap();
            if again.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(again.is_some(), "释放后应能再次拿到");
    }
}
