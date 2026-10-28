pub(crate) const INPUT_TOKEN_SEMANTICS_LEGACY: i64 = 0;
pub(crate) const INPUT_TOKEN_SEMANTICS_TOTAL: i64 = 1;
pub(crate) fn is_cache_inclusive_app(app: &str) -> bool {
    matches!(app, "codex" | "gemini" | "grokbuild")
}
