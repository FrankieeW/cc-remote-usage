use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Claude usage 中按 1 小时 TTL 写入的缓存 token（`cache_creation_input_tokens` 的子集）
pub(crate) fn claude_cache_creation_1h_tokens(usage: &Value) -> u32 {
    usage
        .pointer("/cache_creation/ephemeral_1h_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

/// Session 日志 request_id 前缀，与 `session_usage.rs` 中的格式保持一致
pub const SESSION_REQUEST_ID_PREFIX: &str = "session:";

/// Claude Code and Claude Desktop share Claude message ids with the session
/// importer, so both use the bare `session:{message_id}` namespace. Other
/// apps retain app/provider scoping to avoid collisions between upstreams.
pub fn dedup_scope_for_app<'a>(
    app_type: &'a str,
    provider_id: &'a str,
) -> Option<(&'a str, &'a str)> {
    (!matches!(app_type, "claude" | "claude-desktop")).then_some((app_type, provider_id))
}

fn response_id(body: &Value, field: &str) -> Option<String> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Token 使用量统计
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
    /// `cache_creation_tokens` 中按 1 小时 TTL 写入的部分（Claude 单独计价）
    pub cache_creation_1h_tokens: u32,
    /// 从响应中提取的实际模型名称（如果可用）
    pub model: Option<String>,
    /// 从响应中提取的消息 ID（用于跨源去重）
    ///
    /// Claude API: `msg_xxx`，与 session JSONL 中的 `message.id` 一致
    #[serde(skip)]
    pub message_id: Option<String>,
}
