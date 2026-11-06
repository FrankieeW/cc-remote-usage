//! Session usage import routines adapted from CC Switch desktop v4.0.21.
#![allow(dead_code)]

mod codex_rollout_file;
mod error {
    #[derive(Debug)]
    pub enum AppError {
        Database(String),
        Config(String),
    }
    impl std::fmt::Display for AppError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Database(s) | Self::Config(s) => f.write_str(s),
            }
        }
    }
    impl std::error::Error for AppError {}
    impl From<rusqlite::Error> for AppError {
        fn from(e: rusqlite::Error) -> Self {
            Self::Database(e.to_string())
        }
    }
}
mod database {
    macro_rules! lock_conn {
        ($conn:expr) => {
            $conn.lock().unwrap_or_else(|e| e.into_inner())
        };
    }
    pub(crate) use lock_conn;
    pub struct Database {
        pub(crate) conn: std::sync::Mutex<rusqlite::Connection>,
    }
}
mod proxy {
    pub mod usage {
        pub mod calculator;
        pub mod parser;
    }
}
mod services {
    pub mod session_usage;
    pub mod session_usage_codex;
    pub mod sql_helpers;
    pub mod usage_stats;
}

use database::Database;
pub use error::AppError;
pub use services::session_usage::SessionSyncResult;
use std::path::Path;

fn real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

fn real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Only schemas audited against the desktop implementation are supported.
///
/// - 23: desktop v4.0.21, the release these routines were extracted from.
/// - 24: desktop v4.0.22–v4.0.26. The migration only adds columns: `enabled_dsh` on
///   `mcp_servers` / `skills` (never touched here) and the nullable DeepSeek Harness
///   parser context `session_log_sync.parser_state`. Upstream still writes Codex and
///   Claude cursors without `parser_state`, exactly like these routines.
const SUPPORTED_SCHEMAS: [i64; 2] = [23, 24];

fn check_schema(conn: &rusqlite::Connection) -> Result<(), AppError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if !SUPPORTED_SCHEMAS.contains(&version) {
        let supported = SUPPORTED_SCHEMAS.map(|v| v.to_string()).join("、");
        return Err(AppError::Database(format!(
            "数据库 schema {version} 与本导入器支持的 schema {supported} 不匹配"
        )));
    }
    // Prepare all required tables before any import can modify data.
    for sql in [
        "SELECT request_id, provider_id, app_type, model, request_model, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, input_token_semantics, input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd, latency_ms, first_token_ms, status_code, error_message, session_id, provider_type, is_streaming, cost_multiplier, created_at, data_source FROM proxy_request_logs LIMIT 0",
        "SELECT file_path, last_modified, last_line_offset, last_synced_at, last_byte_offset, last_tail_fingerprint FROM session_log_sync LIMIT 0",
        "SELECT model_id, input_cost_per_million, output_cost_per_million, cache_read_cost_per_million, cache_creation_cost_per_million FROM model_pricing LIMIT 0",
        "SELECT data_source, request_id, semantic_id, has_entry_id FROM session_usage_dedup LIMIT 0",
    ] {
        conn.prepare(sql)?;
    }
    // A schema 24 database is only the audited one when it carries the column added by v24.
    if version >= 24 {
        conn.prepare("SELECT parser_state FROM session_log_sync LIMIT 0")
            .map_err(|e| {
                AppError::Database(format!(
                    "schema {version} 数据库缺少 session_log_sync.parser_state 列，与已审计的结构不符：{e}"
                ))
            })?;
    }
    Ok(())
}

/// Verify database compatibility without modifying or creating it.
pub fn preflight(database: &Path) -> Result<(), AppError> {
    if !database.is_absolute() {
        return Err(AppError::Config("数据库路径必须是绝对路径".into()));
    }
    let conn = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    check_schema(&conn)
}

/// Import one app from an explicitly supplied mirror into an existing CC Switch database.
/// The caller is responsible for serializing imports and checking file-level errors.
pub fn import(database: &Path, app: &str, mirror: &Path) -> Result<SessionSyncResult, AppError> {
    if !database.is_absolute() || !mirror.is_absolute() {
        return Err(AppError::Config("数据库和镜像路径必须是绝对路径".into()));
    }
    if !matches!(app, "codex" | "claude") {
        return Err(AppError::Config(format!("不支持的应用：{app}")));
    }
    let conn = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    check_schema(&conn)?;
    let db = Database {
        conn: std::sync::Mutex::new(conn),
    };
    match app {
        "codex" => services::session_usage_codex::sync_codex_usage(&db, mirror),
        "claude" => services::session_usage::sync_claude_session_logs(&db, mirror),
        _ => unreachable!(),
    }
}
