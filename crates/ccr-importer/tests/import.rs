use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    db: PathBuf,
    codex: PathBuf,
    claude: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cc-switch.db");
        let codex = dir.path().join("codex");
        let claude = dir.path().join("claude");
        fs::create_dir_all(codex.join("sessions/2026/07/10")).unwrap();
        fs::create_dir_all(claude.join("projects/project")).unwrap();
        let c = Connection::open(&db).unwrap();
        c.execute_batch("PRAGMA user_version=23;
CREATE TABLE proxy_request_logs (
request_id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, app_type TEXT NOT NULL, model TEXT NOT NULL,
request_model TEXT, input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
input_token_semantics INTEGER NOT NULL DEFAULT 0, input_cost_usd TEXT NOT NULL DEFAULT '0',
output_cost_usd TEXT NOT NULL DEFAULT '0', cache_read_cost_usd TEXT NOT NULL DEFAULT '0',
cache_creation_cost_usd TEXT NOT NULL DEFAULT '0', total_cost_usd TEXT NOT NULL DEFAULT '0',
latency_ms INTEGER NOT NULL, first_token_ms INTEGER, status_code INTEGER NOT NULL, error_message TEXT,
session_id TEXT, provider_type TEXT, is_streaming INTEGER NOT NULL DEFAULT 0,
cost_multiplier TEXT NOT NULL DEFAULT '1.0', created_at INTEGER NOT NULL, data_source TEXT NOT NULL DEFAULT 'proxy');
CREATE TABLE session_log_sync (file_path TEXT PRIMARY KEY, last_modified INTEGER NOT NULL,
last_line_offset INTEGER NOT NULL DEFAULT 0, last_synced_at INTEGER NOT NULL, last_byte_offset INTEGER, last_tail_fingerprint INTEGER);
CREATE TABLE model_pricing (model_id TEXT PRIMARY KEY, display_name TEXT NOT NULL, input_cost_per_million TEXT NOT NULL,
output_cost_per_million TEXT NOT NULL, cache_read_cost_per_million TEXT NOT NULL DEFAULT '0', cache_creation_cost_per_million TEXT NOT NULL DEFAULT '0');
CREATE TABLE session_usage_dedup (data_source TEXT NOT NULL, request_id TEXT NOT NULL, semantic_id TEXT NOT NULL, has_entry_id INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(data_source,request_id));
INSERT INTO model_pricing VALUES ('gpt-test','test','2','8','0.2','2');
INSERT INTO model_pricing VALUES ('claude-test','test','3','15','0.3','3.75');").unwrap();
        drop(c);
        Self {
            _dir: dir,
            db,
            codex,
            claude,
        }
    }
    fn sql_i64(&self, sql: &str) -> i64 {
        Connection::open(&self.db)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }
    fn codex_file(&self, id: &str) -> PathBuf {
        self.codex.join(format!(
            "sessions/2026/07/10/rollout-2026-07-10T03-00-00-{id}.jsonl"
        ))
    }
}
fn write_lines(path: &Path, lines: &[Value]) {
    let mut f = fs::File::create(path).unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
}
fn meta(id: &str, parent: Option<&str>) -> Value {
    let source = match parent {
        Some(p) => json!({"subagent":{"thread_spawn":{"parent_thread_id":p}}}),
        None => json!("cli"),
    };
    json!({"timestamp":"2026-07-10T03:00:00Z","type":"session_meta","payload":{"id":id,"source":source}})
}
fn context() -> Value {
    json!({"timestamp":"2026-07-10T03:00:01Z","type":"turn_context","payload":{"model":"gpt-test"}})
}
fn tokens(input: u64, cached: u64, output: u64, sec: &str) -> Value {
    json!({"timestamp":sec,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":input,"cached_input_tokens":cached,"output_tokens":output,"reasoning_output_tokens":0,"total_tokens":input+output}}}})
}
const ID: &str = "00000000-0000-4000-8000-000000000001";
const CHILD: &str = "00000000-0000-4000-8000-000000000002";

#[test]
fn codex_incremental_import_is_idempotent_and_priced() {
    let f = Fixture::new();
    let file = f.codex_file(ID);
    write_lines(
        &file,
        &[
            meta(ID, None),
            context(),
            tokens(1000, 400, 100, "2026-07-10T03:00:02Z"),
        ],
    );
    let r = ccr_importer::import(&f.db, "codex", &f.codex).unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.imported, 1);
    let c = Connection::open(&f.db).unwrap();
    let row: (i64,i64,i64,String) = c.query_row("SELECT input_tokens,cache_read_tokens,output_tokens,total_cost_usd FROM proxy_request_logs",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!((row.0, row.1, row.2), (1000, 400, 100));
    assert!(row.3.parse::<f64>().unwrap() > 0.0);
    assert_eq!(
        ccr_importer::import(&f.db, "codex", &f.codex)
            .unwrap()
            .imported,
        0
    );
    writeln!(
        fs::OpenOptions::new().append(true).open(file).unwrap(),
        "{}",
        tokens(1200, 500, 125, "2026-07-10T03:00:03Z")
    )
    .unwrap();
    assert_eq!(
        ccr_importer::import(&f.db, "codex", &f.codex)
            .unwrap()
            .imported,
        1
    );
    assert_eq!(
        f.sql_i64("SELECT SUM(input_tokens) FROM proxy_request_logs"),
        1200
    );
}

#[test]
fn missing_parent_is_deferred_until_parent_arrives() {
    let f = Fixture::new();
    write_lines(
        &f.codex_file(CHILD),
        &[
            meta(CHILD, Some(ID)),
            context(),
            tokens(1000, 400, 100, "2026-07-10T03:00:02Z"),
        ],
    );
    let r = ccr_importer::import(&f.db, "codex", &f.codex).unwrap();
    assert_eq!(r.deferred_files, 1);
    assert_eq!(f.sql_i64("SELECT COUNT(*) FROM proxy_request_logs"), 0);
    assert_eq!(f.sql_i64("SELECT COUNT(*) FROM session_log_sync"), 0);
    write_lines(
        &f.codex_file(ID),
        &[
            meta(ID, None),
            context(),
            tokens(500, 200, 50, "2026-07-10T02:59:58Z"),
        ],
    );
    let r = ccr_importer::import(&f.db, "codex", &f.codex).unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.deferred_files, 0);
}

#[test]
fn schema_mismatch_and_write_errors_do_not_advance_cursor() {
    let f = Fixture::new();
    write_lines(
        &f.codex_file(ID),
        &[
            meta(ID, None),
            context(),
            tokens(1000, 400, 100, "2026-07-10T03:00:02Z"),
        ],
    );
    let c = Connection::open(&f.db).unwrap();
    for unsupported in [22, 25] {
        c.execute_batch(&format!("PRAGMA user_version={unsupported};"))
            .unwrap();
        let err = ccr_importer::preflight(&f.db).unwrap_err().to_string();
        assert!(err.contains(&format!("schema {unsupported}")), "{err}");
        assert!(ccr_importer::import(&f.db, "codex", &f.codex).is_err());
    }
    c.execute_batch("PRAGMA user_version=23; CREATE TRIGGER reject_import BEFORE INSERT ON proxy_request_logs BEGIN SELECT RAISE(FAIL,'blocked'); END;").unwrap();
    let r = ccr_importer::import(&f.db, "codex", &f.codex).unwrap();
    assert!(!r.errors.is_empty());
    assert_eq!(f.sql_i64("SELECT COUNT(*) FROM session_log_sync"), 0);
    assert_eq!(f.sql_i64("SELECT COUNT(*) FROM proxy_request_logs"), 0);
}

/// Desktop v4.0.22 migration 23 → 24 as far as the importer's tables are concerned:
/// `session_log_sync` only gains the nullable DSH parser context column.
fn upgrade_to_schema_24(db: &Path) {
    Connection::open(db)
        .unwrap()
        .execute_batch(
            "ALTER TABLE session_log_sync ADD COLUMN parser_state TEXT;
             PRAGMA user_version=24;",
        )
        .unwrap();
}

#[test]
fn schema_24_imports_both_apps_without_touching_desktop_parser_state() {
    let f = Fixture::new();
    upgrade_to_schema_24(&f.db);
    // A cursor owned by the desktop DeepSeek Harness importer must survive our writes.
    Connection::open(&f.db)
        .unwrap()
        .execute_batch(
            "INSERT INTO session_log_sync
               (file_path, last_modified, last_line_offset, last_synced_at, last_byte_offset, parser_state)
             VALUES ('dsh:/sessions/a.jsonl', 1, 0, 1, 42, '{\"pending\":true}');",
        )
        .unwrap();
    ccr_importer::preflight(&f.db).unwrap();
    write_lines(
        &f.codex_file(ID),
        &[
            meta(ID, None),
            context(),
            tokens(1000, 400, 100, "2026-07-10T03:00:02Z"),
        ],
    );
    write_lines(
        &f.claude.join("projects/project/session.jsonl"),
        &[
            json!({"type":"assistant","timestamp":"2026-07-10T03:00:00Z","sessionId":"s1","message":{"id":"msg-v24","model":"claude-test","stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":50,"cache_creation_input_tokens":10}}}),
        ],
    );
    for (app, mirror) in [("codex", &f.codex), ("claude", &f.claude)] {
        let r = ccr_importer::import(&f.db, app, mirror).unwrap();
        assert!(r.errors.is_empty(), "{app}: {:?}", r.errors);
        assert_eq!(r.imported, 1, "{app}");
        let again = ccr_importer::import(&f.db, app, mirror).unwrap();
        assert_eq!(again.imported, 0, "{app} must stay idempotent");
    }
    assert_eq!(f.sql_i64("SELECT COUNT(*) FROM proxy_request_logs"), 2);
    assert!(
        f.sql_i64("SELECT COUNT(*) FROM session_log_sync WHERE file_path NOT LIKE 'dsh:%'") >= 2
    );
    assert_eq!(
        f.sql_i64("SELECT COUNT(*) FROM session_log_sync WHERE file_path NOT LIKE 'dsh:%' AND parser_state IS NOT NULL"),
        0
    );
    let kept: (i64, String) = Connection::open(&f.db)
        .unwrap()
        .query_row(
            "SELECT last_byte_offset, parser_state FROM session_log_sync WHERE file_path='dsh:/sessions/a.jsonl'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(kept, (42, "{\"pending\":true}".to_string()));
}

#[test]
fn schema_24_without_parser_state_is_not_the_audited_schema() {
    let f = Fixture::new();
    write_lines(
        &f.codex_file(ID),
        &[
            meta(ID, None),
            context(),
            tokens(1000, 400, 100, "2026-07-10T03:00:02Z"),
        ],
    );
    Connection::open(&f.db)
        .unwrap()
        .execute_batch("PRAGMA user_version=24;")
        .unwrap();
    let err = ccr_importer::preflight(&f.db).unwrap_err().to_string();
    assert!(err.contains("parser_state"), "{err}");
    assert!(ccr_importer::import(&f.db, "codex", &f.codex).is_err());
    assert_eq!(f.sql_i64("SELECT COUNT(*) FROM session_log_sync"), 0);
    assert_eq!(f.sql_i64("SELECT COUNT(*) FROM proxy_request_logs"), 0);
}

#[test]
fn claude_repeated_blocks_are_one_request_and_updates_are_idempotent() {
    let f = Fixture::new();
    let file = f.claude.join("projects/project/session.jsonl");
    let msg = |out, stop| json!({"type":"assistant","timestamp":"2026-07-10T03:00:00Z","sessionId":"s1","message":{"id":"msg-one","model":"claude-test","stop_reason":stop,"usage":{"input_tokens":100,"output_tokens":out,"cache_read_input_tokens":50,"cache_creation_input_tokens":10}}});
    write_lines(&file, &[msg(1, Value::Null), msg(20, json!("end_turn"))]);
    let r = ccr_importer::import(&f.db, "claude", &f.claude).unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.imported, 1);
    assert_eq!(
        f.sql_i64(
            "SELECT output_tokens FROM proxy_request_logs WHERE request_id='session:msg-one'"
        ),
        20
    );
    assert_eq!(
        ccr_importer::import(&f.db, "claude", &f.claude)
            .unwrap()
            .imported,
        0
    );
}

#[test]
fn remote_compaction_usage_arrives_before_zero_snapshot_and_is_imported_once() {
    let f = Fixture::new();
    let file = f.codex_file(ID);
    let record = json!({"timestamp":"2026-07-10T03:00:02Z","type":"token_usage_record","payload":{"response_id":"compact-1","usage":{"input_tokens":26705,"cached_input_tokens":0,"output_tokens":491,"total_tokens":27196}}});
    write_lines(&file, &[meta(ID, None), context(), record]);
    assert_eq!(
        ccr_importer::import(&f.db, "codex", &f.codex)
            .unwrap()
            .imported,
        0
    );
    let zero = json!({"timestamp":"2026-07-10T03:05:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"total_tokens":0},"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"total_tokens":0}}}});
    writeln!(
        fs::OpenOptions::new().append(true).open(&file).unwrap(),
        "{zero}"
    )
    .unwrap();
    let second = ccr_importer::import(&f.db, "codex", &f.codex).unwrap();
    assert_eq!(second.imported, 1, "{:?}", second.errors);
    assert_eq!(
        f.sql_i64("SELECT input_tokens FROM proxy_request_logs"),
        26705
    );
    writeln!(
        fs::OpenOptions::new().append(true).open(&file).unwrap(),
        "{zero}"
    )
    .unwrap();
    assert_eq!(
        ccr_importer::import(&f.db, "codex", &f.codex)
            .unwrap()
            .imported,
        0
    );
    assert_eq!(f.sql_i64("SELECT COUNT(*) FROM proxy_request_logs"), 1);
}

#[cfg(unix)]
#[test]
fn symlinked_mirror_files_are_not_imported() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let outside = f._dir.path().join("outside.jsonl");
    write_lines(
        &outside,
        &[
            meta(ID, None),
            context(),
            tokens(1000, 400, 100, "2026-07-10T03:00:02Z"),
        ],
    );
    symlink(outside, f.codex_file(ID)).unwrap();
    let result = ccr_importer::import(&f.db, "codex", &f.codex).unwrap();
    assert_eq!(result.imported, 0);
    assert_eq!(f.sql_i64("SELECT COUNT(*) FROM proxy_request_logs"), 0);
}
