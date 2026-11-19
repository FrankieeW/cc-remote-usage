//! Read-only projection of Codex requests from one host's mirror.

use super::{
    build_rollout_index, matching_replay_prefix, parse_codex_file_with_index,
    resolve_parent_signatures, thread_id_from_filename, ParentResolution, ParseMode,
    CODEX_THREAD_REQUEST_ID_PREFIX,
};
use std::path::PathBuf;

pub struct CodexReportUsage {
    pub record_key: Option<String>,
    pub snapshot_key: Option<String>,
    pub timestamp: String,
    pub model: String,
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_creation: u32,
}

/// The full mirror is needed to resolve forked histories and continuation baselines.
/// Like the importer, leave unresolved parented rollouts out of the totals.
pub fn scan_codex_report(files: &[PathBuf], mut emit: impl FnMut(CodexReportUsage)) {
    let index = build_rollout_index(files);
    for file in files {
        let Ok(parsed) = parse_codex_file_with_index(
            file,
            thread_id_from_filename(file),
            Some(&index),
            ParseMode::Report,
        ) else {
            continue;
        };
        let replay_prefix = match &parsed.parent {
            // Record-only logs have no snapshot history to align. Their response
            // IDs still allow cross-mirror dedup, even without the parent file.
            _ if parsed.token_events.is_empty() => 0,
            ParentResolution::None | ParentResolution::Continuation { .. } => 0,
            ParentResolution::Deferred(_) => continue,
            ParentResolution::Parent(parent_id) => {
                let Some(cutoff) = parsed.root_timestamp else {
                    continue;
                };
                let Ok(parent) = resolve_parent_signatures(parent_id, cutoff, &index) else {
                    continue;
                };
                matching_replay_prefix(&parsed.token_events, &parent)
            }
        };
        let replay_end = parsed.token_events[..replay_prefix]
            .last()
            .map_or(0, |e| e.line_offset);
        let rollout_id = parsed.root_thread_id.or(parsed.meta_thread_id);
        let snapshots = parsed
            .token_events
            .into_iter()
            .skip(replay_prefix)
            .filter(|e| e.event_index.is_some());
        let records = parsed
            .usage_record_events
            .into_iter()
            .filter(|e| e.line_offset > replay_end);
        for event in snapshots.chain(records) {
            let Some(timestamp) = event.timestamp else {
                continue;
            };
            let snapshot_key = event
                .event_index
                .zip(rollout_id.as_deref())
                .map(|(ordinal, id)| format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{id}:{ordinal}"));
            let delta = event.delta;
            let mut input = delta.input - delta.cached_input;
            if event.event_index.is_some() {
                // Snapshot input includes both cache reads and writes. Records alone
                // retain the report's historical cache split and pricing.
                input -= delta.cache_write_input;
            }
            emit(CodexReportUsage {
                record_key: event.request_suffix,
                snapshot_key,
                timestamp,
                model: event.model,
                input,
                output: delta.output,
                cache_read: delta.cached_input,
                cache_creation: delta.cache_write_input,
            });
        }
    }
}
