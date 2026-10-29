// PlaywrightImporter — reads a Playwright trace.zip and creates a non-replayable
// Loom session on disk.
//
// # Contract semantics
// - Opens the zip in-memory; locates the entry whose name ends with "trace.trace".
// - Counts K = number of non-empty lines (NDJSON events) in that file.
// - Generates a ULID session_id; creates `sessions_root/<session_id>/`.
// - Writes a WAL (`manifest.wal`) with:
//     Header + K ActionReceipt entries (source="playwright_import") + SessionTerminal.
// - Writes `session_meta.json` (atomically via temp-file + rename):
//     {"replayable": false, "source": "playwright_import", "action_count": K}
// - Returns ImportResult { session_id, action_count: K }.
//
// # Known limitations
// - Hash chain uses literal "0"*64 placeholders for prev_hash. Import sessions
//   are non-replayable; `validate()` will return ManifestCorrupt on them by design.
// - Sessions are written directly to disk; the in-memory SessionManager is NOT
//   updated. Sessions are visible to daemon queries only after daemon restart.
//   This is acceptable for a migration tool.

use crate::error::{LoomError, LoomErrorCode};
use crate::manifest_writer::ManifestEntry;
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read as _};
use std::path::PathBuf;
use ulid::Ulid;

/// Maximum decompressed size of `trace.trace` accepted without error (100 MB).
const MAX_TRACE_BYTES: u64 = 100_000_000;

/// Result returned by a successful import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub session_id: String,
    pub action_count: u64,
}

/// Imports Playwright trace zips as non-replayable Loom sessions.
pub struct PlaywrightImporter {
    sessions_root: PathBuf,
}

impl PlaywrightImporter {
    pub fn new(sessions_root: PathBuf) -> Self {
        Self { sessions_root }
    }

    /// Import a Playwright `trace.zip` from raw bytes.
    ///
    /// Returns `ImportResult` on success; `Err(LoomError)` on invalid zip,
    /// missing `trace.trace` entry, or I/O failure.
    pub fn import(&self, trace_bytes: &[u8]) -> Result<ImportResult, LoomError> {
        // --- 1. Open zip and locate trace.trace ---
        let cursor = Cursor::new(trace_bytes);
        let mut archive = zip::ZipArchive::new(cursor).map_err(|e| {
            LoomError::new(LoomErrorCode::InvalidArgument, format!("invalid zip: {e}"))
        })?;

        // Find entry whose name ends with "trace.trace" (handles path-prefixed variants).
        let trace_entry_name: Option<String> = {
            let mut found = None;
            for i in 0..archive.len() {
                let entry = archive.by_index(i).map_err(|e| {
                    LoomError::new(LoomErrorCode::Io, format!("zip read error: {e}"))
                })?;
                // zip 9 made `name()` fallible (an entry name need not be valid
                // UTF-8). An entry we cannot name is not the one we are looking
                // for, so skip it rather than failing the whole import over an
                // unrelated oddly-named attachment.
                let Ok(name) = entry.name() else {
                    continue;
                };
                if name.ends_with("trace.trace") {
                    found = Some(name.into_owned());
                    break;
                }
            }
            found
        };

        let trace_entry_name = trace_entry_name.ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::InvalidArgument,
                "trace.zip does not contain a trace.trace entry",
            )
        })?;

        // Re-open by name; check decompressed size before reading.
        let mut trace_file = archive
            .by_name(&trace_entry_name)
            .map_err(|e| LoomError::new(LoomErrorCode::Io, format!("zip entry open error: {e}")))?;

        if trace_file.size() > MAX_TRACE_BYTES {
            return Err(LoomError::new(
                LoomErrorCode::InvalidArgument,
                format!(
                    "trace.trace is too large ({} bytes > {} byte limit)",
                    trace_file.size(),
                    MAX_TRACE_BYTES
                ),
            ));
        }

        let mut trace_content = String::new();
        trace_file.read_to_string(&mut trace_content).map_err(|e| {
            LoomError::new(LoomErrorCode::Io, format!("trace.trace read error: {e}"))
        })?;

        // --- 2. Count K = non-empty lines ---
        // Each non-empty line is one Playwright event.
        let events: Vec<&str> = trace_content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let k = events.len() as u64;

        // --- 3. Generate session_id and create session directory ---
        let session_id = Ulid::generate().to_string().to_lowercase();
        let session_dir = self.sessions_root.join(&session_id);
        std::fs::create_dir_all(&session_dir)?;

        // --- 4. Write manifest.wal ---
        let now_ms = now_ms();
        let wal_path = session_dir.join("manifest.wal");
        let wal_lines = build_wal_lines(&session_id, now_ms, k);
        std::fs::write(&wal_path, wal_lines.join("\n"))?;

        // --- 5. Write session_meta.json atomically (temp + rename) ---
        let meta = serde_json::json!({
            "replayable": false,
            "source": "playwright_import",
            "action_count": k,
        });
        let meta_bytes =
            serde_json::to_vec(&meta).map_err(|e| LoomError::internal(e.to_string()))?;
        let meta_tmp = session_dir.join("session_meta.json.tmp");
        std::fs::write(&meta_tmp, &meta_bytes)?;
        std::fs::rename(&meta_tmp, session_dir.join("session_meta.json"))?;

        Ok(ImportResult {
            session_id,
            action_count: k,
        })
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Current Unix timestamp in milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Build NDJSON WAL lines: Header + K ActionReceipts + SessionTerminal.
///
/// Uses `"0".repeat(64)` as `prev_hash` placeholder — import sessions are
/// non-replayable so hash-chain integrity is not required.
/// `validate()` will return ManifestCorrupt on import sessions by design.
fn build_wal_lines(session_id: &str, now_ms: u64, k: u64) -> Vec<String> {
    let zero_hash = "0".repeat(64);
    let mut lines = Vec::with_capacity((k + 2) as usize);

    // Header
    let header = ManifestEntry::Header {
        session_id: session_id.to_string(),
        started_at_ms: now_ms,
        prev_hash: None,
        budgets: None,
        capture_policy: None,
        // Imported (non-loom) sessions have no determinism seed.
        seed: None,
        // Imported sessions are non-replayable already; leave determinism
        // unspecified (legacy/None) — the replay refuse-guard is moot here.
        determinism_enabled: None,
    };
    lines.push(serde_json::to_string(&header).expect("Header serialisation must not fail"));

    // K ActionReceipts
    for i in 0..k {
        let receipt_bytes = serde_json::to_vec(&serde_json::json!({
            "source": "playwright_import",
            "event_index": i,
        }))
        .expect("receipt_canonical_bytes serialisation must not fail");

        let entry = ManifestEntry::ActionReceipt {
            action_id: i + 1,
            emitted_at_ms: now_ms + i,
            receipt_canonical_bytes: receipt_bytes,
            prev_hash: zero_hash.clone(),
        };
        lines.push(
            serde_json::to_string(&entry).expect("ActionReceipt serialisation must not fail"),
        );
    }

    // SessionTerminal
    let terminal = ManifestEntry::SessionTerminal {
        action_id: k + 1,
        emitted_at_ms: now_ms + k,
        reason: "import_complete".to_string(),
        prev_hash: zero_hash,
    };
    lines.push(
        serde_json::to_string(&terminal).expect("SessionTerminal serialisation must not fail"),
    );

    lines
}
