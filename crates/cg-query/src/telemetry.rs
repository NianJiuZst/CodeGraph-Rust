//! Opt-in structured JSONL telemetry for CLI and MCP tool calls (`CGR_TELEMETRY_LOG`).
//! Dormant with zero disk I/O unless `CGR_TELEMETRY_LOG` is set to a non-empty file path.

use crate::tools::ToolMeta;
use cg_core::brand;
use serde::Serialize;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
pub struct TelemetryEvent<'a> {
    pub ts_ms: u64,
    pub mode: &'a str,
    pub tool: &'a str,
    pub project: &'a str,
    pub duration_ms: f64,
    pub sync_checked: bool,
    pub synced: bool,
    pub sync_ms: f64,
    pub generation: u64,
    pub output_bytes: usize,
    pub compact: bool,
    pub max_chars: Option<usize>,
    pub one_symbol: bool,
    pub dedup_enabled: bool,
    pub dedup_hits: u32,
    pub dedup_saved_bytes: usize,
    pub tail_idx_checked: bool,
    pub tail_idx_triggered: bool,
    pub tail_idx_ambiguous: usize,
    pub tail_idx_dynamic: usize,
    pub tail_idx_no_candidate: usize,
    pub is_error: bool,
}

pub struct CallStats<'a> {
    pub mode: &'a str,
    pub tool: &'a str,
    pub project: &'a Path,
    pub duration_ms: f64,
    pub sync_checked: bool,
    pub synced: bool,
    pub sync_ms: f64,
    pub generation: u64,
    pub output_bytes: usize,
    pub compact: bool,
    pub max_chars: Option<usize>,
    pub dedup_enabled: bool,
    pub meta: ToolMeta,
    pub is_error: bool,
}

pub fn record(stats: CallStats<'_>) {
    let Some(path) = brand::env::get(brand::env::TELEMETRY_LOG, None) else {
        return;
    };
    let ts_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let project_str = stats.project.to_string_lossy();
    let round2 = |v: f64| (v * 100.0).round() / 100.0;
    let ev = TelemetryEvent {
        ts_ms,
        mode: stats.mode,
        tool: stats.tool,
        project: &project_str,
        duration_ms: round2(stats.duration_ms),
        sync_checked: stats.sync_checked,
        synced: stats.synced,
        sync_ms: round2(stats.sync_ms),
        generation: stats.generation,
        output_bytes: stats.output_bytes,
        compact: stats.compact,
        max_chars: stats.max_chars,
        one_symbol: stats.meta.one_symbol,
        dedup_enabled: stats.dedup_enabled,
        dedup_hits: stats.meta.dedup_hits,
        dedup_saved_bytes: stats.meta.dedup_saved_bytes,
        tail_idx_checked: stats.meta.tail_idx_checked,
        tail_idx_triggered: stats.meta.tail_idx_triggered,
        tail_idx_ambiguous: stats.meta.tail_idx_ambiguous,
        tail_idx_dynamic: stats.meta.tail_idx_dynamic,
        tail_idx_no_candidate: stats.meta.tail_idx_no_candidate,
        is_error: stats.is_error,
    };
    let Ok(mut line) = serde_json::to_string(&ev) else {
        return;
    };
    line.push('\n');
    let p = Path::new(&path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = f.write_all(line.as_bytes());
    }
}
