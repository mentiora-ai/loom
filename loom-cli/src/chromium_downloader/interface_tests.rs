// Interface tests for `ChromiumDownloader`. Verifies the
// supply-chain mismatch shape, the idempotent-ensure contract (sentinel
// file model), extraction, and the
// `DoctorRunner`-shared verify-only path.

use super::chromium_downloader::{
    copy_with_progress, sha256_of_file, ChromiumDownloader, ChromiumDownloaderConfig,
    DownloadOutcome, ProgressReporter, PROGRESS_CHUNK_SIZE,
};
use crate::CliError;

// ── Structural / compile-time tests ─────────────────────────────────────────

#[test]
fn config_carries_install_dir_and_binary_subpath() {
    let c = ChromiumDownloaderConfig {
        install_dir: "/tmp/chromium".into(),
        binary_subpath: "rev1/Chromium".into(),
    };
    assert!(c.install_dir.is_absolute());
}

#[test]
fn binary_path_joins_install_dir_and_subpath() {
    let d = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: "/tmp/chromium".into(),
        binary_subpath: "rev1/Chromium".into(),
    });
    let p = d.binary_path();
    assert_eq!(p, std::path::PathBuf::from("/tmp/chromium/rev1/Chromium"));
}

#[test]
fn download_outcome_variant_set_locked() {
    fn _ck(o: DownloadOutcome) -> &'static str {
        match o {
            DownloadOutcome::Skipped => "skipped",
            DownloadOutcome::Downloaded(_) => "downloaded",
        }
    }
    let _ = _ck;
}

#[test]
fn ensure_signature() {
    fn _ck(d: &ChromiumDownloader, url: &str, sha: &str) {
        let _f = async move {
            let _: Result<DownloadOutcome, CliError> = d.ensure(url, sha).await;
        };
    }
    let _ = _ck;
}

#[test]
fn verify_signature() {
    fn _ck(d: &ChromiumDownloader, sha: &str) {
        let _f = async move {
            let _: Result<(), CliError> = d.verify(sha).await;
        };
    }
    let _ = _ck;
}

#[test]
fn sha256_of_file_signature() {
    fn _ck(p: &std::path::Path) {
        let _f = async move {
            let _: Result<String, CliError> = sha256_of_file(p).await;
        };
    }
    let _ = _ck;
}

// ── ensure() idempotent via sentinel file ────────────────────

/// When install_dir/.archive_sha256 contains the expected SHA-256 AND the
/// binary exists, ensure() must return Skipped without downloading.
#[tokio::test]
async fn ensure_skips_when_sentinel_matches() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();

    let expected_sha = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";

    // Binary placeholder must exist.
    let binary_path = dir.path().join("Chromium");
    std::fs::write(&binary_path, b"fake-chromium-binary").unwrap();

    // Sentinel with matching SHA.
    let sentinel = dir.path().join(".archive_sha256");
    std::fs::write(&sentinel, expected_sha).unwrap();

    let downloader = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: dir.path().to_path_buf(),
        binary_subpath: "Chromium".into(),
    });

    let result = downloader
        .ensure(
            "https://should-not-be-reached.invalid/chromium.zip",
            expected_sha,
        )
        .await;

    assert!(
        matches!(result, Ok(DownloadOutcome::Skipped)),
        "expected Skipped (sentinel matches), got: {result:?}"
    );
}

// ── ensure() extracts archive after SHA-256 verify ───────────

/// Build a minimal valid zip served via file:// URL. After ensure(), the
/// extracted file must be present and the sentinel must contain the archive SHA.
#[tokio::test]
async fn ensure_refuses_a_zip_entry_that_escapes_the_install_dir() {
    use tempfile::TempDir;

    // An archive entry named `../` writes outside the destination unless the
    // extractor confines it. `extract_zip` does its own extraction rather than
    // calling zip's `extract()`, so the confinement is ours to enforce (the
    // hazard behind RUSTSEC-2026-0367/0368/0369).
    let zip_dir = TempDir::new().unwrap();
    let zip_path = zip_dir.path().join("evil.zip");
    {
        use std::io::Write as _;
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zw = zip::ZipWriter::new(zip_file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        // `start_file_from_path` normalises `..` away, so the raw string API is
        // what carries the traversal name a hostile archive would hold.
        zw.start_file("../escaped.txt", opts).unwrap();
        zw.write_all(b"should never be written").unwrap();
        zw.finish().unwrap();
    }

    let archive_sha = sha256_of_file(&zip_path).await.unwrap();
    let install_root = TempDir::new().unwrap();
    let install_dir = install_root.path().join("chromium");
    let downloader = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: install_dir.clone(),
        binary_subpath: "Chromium".into(),
    });

    let file_url = format!("file://{}", zip_path.display());
    let result = downloader.ensure(&file_url, &archive_sha).await;

    assert!(
        result.is_err(),
        "an entry escaping the install dir must be refused, got: {result:?}"
    );
    assert!(
        !install_root.path().join("escaped.txt").exists(),
        "the traversal entry must not have been written outside the install dir"
    );
}

#[tokio::test]
async fn ensure_extracts_zip_and_writes_sentinel() {
    use tempfile::TempDir;

    // Build a minimal zip containing a file at "Chromium" inside the archive.
    let zip_dir = TempDir::new().unwrap();
    let zip_path = zip_dir.path().join("chromium.zip");
    {
        use std::io::Write as _;
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zw = zip::ZipWriter::new(zip_file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);
        zw.start_file("Chromium", opts).unwrap();
        zw.write_all(b"fake-chromium-binary-content").unwrap();
        zw.finish().unwrap();
    }

    // Compute the real SHA-256 of the zip archive.
    let archive_sha = sha256_of_file(&zip_path).await.unwrap();

    let install_dir = TempDir::new().unwrap();
    let downloader = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: install_dir.path().to_path_buf(),
        binary_subpath: "Chromium".into(),
    });

    let file_url = format!("file://{}", zip_path.display());
    let result = downloader.ensure(&file_url, &archive_sha).await;

    assert!(
        matches!(result, Ok(DownloadOutcome::Downloaded(_))),
        "expected Downloaded, got: {result:?}"
    );

    // binary extracted (real file, not zip archive).
    let binary = install_dir.path().join("Chromium");
    assert!(binary.exists(), "binary must exist after extraction");
    let content = std::fs::read(&binary).unwrap();
    assert_eq!(
        content, b"fake-chromium-binary-content",
        "extracted content must match"
    );

    // sentinel written after extraction.
    let sentinel = install_dir.path().join(".archive_sha256");
    assert!(
        sentinel.exists(),
        "sentinel .archive_sha256 must be written"
    );
    let recorded = std::fs::read_to_string(&sentinel).unwrap();
    assert_eq!(
        recorded.trim(),
        archive_sha,
        "sentinel must contain archive SHA-256"
    );
}

/// SHA mismatch → SupplyChain error; binary must NOT be extracted.
#[tokio::test]
async fn ensure_supply_chain_on_sha_mismatch() {
    use tempfile::TempDir;

    let zip_dir = TempDir::new().unwrap();
    let zip_path = zip_dir.path().join("chromium.zip");
    {
        use std::io::Write as _;
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zw = zip::ZipWriter::new(zip_file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("Chromium", opts).unwrap();
        zw.write_all(b"real-content").unwrap();
        zw.finish().unwrap();
    }

    let install_dir = TempDir::new().unwrap();
    let downloader = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: install_dir.path().to_path_buf(),
        binary_subpath: "Chromium".into(),
    });

    let file_url = format!("file://{}", zip_path.display());
    let wrong_sha = "0000000000000000000000000000000000000000000000000000000000000000";
    let result = downloader.ensure(&file_url, wrong_sha).await;

    assert!(
        matches!(result, Err(CliError::SupplyChain { .. })),
        "expected SupplyChain error, got: {result:?}"
    );

    // Binary must NOT be present after SHA mismatch.
    let binary = install_dir.path().join("Chromium");
    assert!(!binary.exists(), "binary must not exist after SHA mismatch");
}

// ── verify() checks binary + sentinel ────────────────────────

/// Binary + matching sentinel → Ok(()).
#[tokio::test]
async fn verify_ok_when_sentinel_matches() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("Chromium"), b"fake-binary").unwrap();

    let expected_sha = "cafebabe00000000cafebabe00000000cafebabe00000000cafebabe00000000";
    std::fs::write(dir.path().join(".archive_sha256"), expected_sha).unwrap();

    let downloader = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: dir.path().to_path_buf(),
        binary_subpath: "Chromium".into(),
    });

    let result = downloader.verify(expected_sha).await;
    assert!(
        result.is_ok(),
        "verify must return Ok when sentinel matches: {result:?}"
    );
}

/// Binary + wrong sentinel → SupplyChainViolation.
#[tokio::test]
async fn verify_supply_chain_on_sentinel_mismatch() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("Chromium"), b"fake-binary").unwrap();
    std::fs::write(
        dir.path().join(".archive_sha256"),
        "aaaa0000aaaa0000aaaa0000aaaa0000aaaa0000aaaa0000aaaa0000aaaa0000",
    )
    .unwrap();

    let downloader = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: dir.path().to_path_buf(),
        binary_subpath: "Chromium".into(),
    });

    let result = downloader
        .verify("bbbb1111bbbb1111bbbb1111bbbb1111bbbb1111bbbb1111bbbb1111bbbb1111")
        .await;

    assert!(
        matches!(result, Err(CliError::SupplyChain { .. })),
        "expected SupplyChain on sentinel mismatch, got: {result:?}"
    );
}

/// Binary exists but no sentinel → Internal (incomplete install).
#[tokio::test]
async fn verify_internal_on_missing_sentinel() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("Chromium"), b"fake-binary").unwrap();
    // No sentinel.

    let downloader = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: dir.path().to_path_buf(),
        binary_subpath: "Chromium".into(),
    });

    let result = downloader
        .verify("cafebabe00000000cafebabe00000000cafebabe00000000cafebabe00000000")
        .await;

    assert!(
        matches!(result, Err(CliError::Internal(_))),
        "expected Internal on missing sentinel, got: {result:?}"
    );
}

// ── verify() falls back to scanning parent dir for executables ───

/// When the literal binary_path is missing but a differently-named executable
/// exists in the same Contents/MacOS/ directory, verify() must return Ok(()).
#[tokio::test]
async fn verify_fallback_scan_finds_differently_named_binary() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();

    // Create Contents/MacOS/ directory with a differently-named executable.
    let macos_dir = dir.path().join("Contents").join("MacOS");
    std::fs::create_dir_all(&macos_dir).unwrap();
    let alt_binary = macos_dir.join("Google Chrome");
    std::fs::write(&alt_binary, b"fake-chrome-binary").unwrap();

    // Make it executable on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&alt_binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // ChromiumDownloader configured for the non-existent 'Chromium' binary.
    let downloader = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: dir.path().to_path_buf(),
        binary_subpath: "Contents/MacOS/Chromium".into(),
    });

    // binary_path() = Contents/MacOS/Chromium — does not exist.
    // Fallback should scan Contents/MacOS/ and find 'Google Chrome'.
    let result = downloader.verify("any-sha-unused").await;
    assert!(
        result.is_ok(),
        "fallback scan must return Ok when executable found in parent dir; got: {result:?}"
    );
}

// ── verify() fails when no executable exists in parent ───────────

/// When the literal binary_path is missing AND the parent directory contains
/// no executable files, verify() must still return Err(Internal).
#[tokio::test]
async fn verify_fails_when_no_executable_in_parent() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();

    // Create Contents/MacOS/ directory with only a non-executable file.
    let macos_dir = dir.path().join("Contents").join("MacOS");
    std::fs::create_dir_all(&macos_dir).unwrap();
    let non_exec = macos_dir.join("README.txt");
    std::fs::write(&non_exec, b"not a binary").unwrap();
    // Do NOT set executable permission — mode defaults to 0o644.

    let downloader = ChromiumDownloader::new(ChromiumDownloaderConfig {
        install_dir: dir.path().to_path_buf(),
        binary_subpath: "Contents/MacOS/Chromium".into(),
    });

    let result = downloader.verify("any-sha-unused").await;
    assert!(
        matches!(result, Err(CliError::Internal(_))),
        "must return Internal when no executable in parent dir; got: {result:?}"
    );
}

// ── ProgressReporter / copy_with_progress ────────────────────────────────────
//
// The progress primitive is transport-agnostic, so these exercise it over
// in-memory `Read`/`Write` pairs — no network, no daemon, no flake (FND-0006).

/// Records every `on_progress` call so assertions can inspect the callback
/// sequence (cumulative bytes + the passed-through total).
#[derive(Default)]
struct RecordingReporter {
    calls: Vec<(u64, Option<u64>)>,
}

impl ProgressReporter for RecordingReporter {
    fn on_progress(&mut self, bytes_done: u64, total: Option<u64>) {
        self.calls.push((bytes_done, total));
    }
}

#[test]
fn copy_with_progress_copies_all_bytes_and_returns_count() {
    let src = vec![7u8; PROGRESS_CHUNK_SIZE + 123];
    let mut reader: &[u8] = &src;
    let mut sink: Vec<u8> = Vec::new();
    let mut rep = RecordingReporter::default();

    let n = copy_with_progress(&mut reader, &mut sink, Some(src.len() as u64), &mut rep).unwrap();

    assert_eq!(n, src.len() as u64, "returns total bytes copied");
    assert_eq!(sink, src, "writer receives the exact source bytes");
}

#[test]
fn copy_with_progress_emits_multiple_updates_for_multichunk_source() {
    // > one chunk → ≥2 callbacks (the AC5 "visible progress" proxy).
    let src = vec![0u8; PROGRESS_CHUNK_SIZE * 3 + 1];
    let mut reader: &[u8] = &src;
    let mut sink: Vec<u8> = Vec::new();
    let mut rep = RecordingReporter::default();

    copy_with_progress(&mut reader, &mut sink, None, &mut rep).unwrap();

    assert!(
        rep.calls.len() >= 2,
        "multi-chunk source must yield ≥2 progress updates; got {}",
        rep.calls.len()
    );
}

#[test]
fn copy_with_progress_reports_monotonic_cumulative_bytes() {
    let src = vec![1u8; PROGRESS_CHUNK_SIZE * 2 + 50];
    let mut reader: &[u8] = &src;
    let mut sink: Vec<u8> = Vec::new();
    let mut rep = RecordingReporter::default();

    copy_with_progress(&mut reader, &mut sink, Some(src.len() as u64), &mut rep).unwrap();

    // Cumulative byte counts strictly increase and the last equals the total.
    let mut prev = 0u64;
    for (done, total) in &rep.calls {
        assert!(
            *done > prev,
            "cumulative bytes must increase: {done} !> {prev}"
        );
        assert_eq!(
            *total,
            Some(src.len() as u64),
            "total is passed through unchanged"
        );
        prev = *done;
    }
    assert_eq!(
        rep.calls.last().map(|(d, _)| *d),
        Some(src.len() as u64),
        "final callback reports the full byte count"
    );
}

#[test]
fn copy_with_progress_empty_source_yields_no_callbacks() {
    let src: Vec<u8> = Vec::new();
    let mut reader: &[u8] = &src;
    let mut sink: Vec<u8> = Vec::new();
    let mut rep = RecordingReporter::default();

    let n = copy_with_progress(&mut reader, &mut sink, Some(0), &mut rep).unwrap();

    assert_eq!(n, 0);
    assert!(sink.is_empty());
    assert!(
        rep.calls.is_empty(),
        "no chunk → no callback (terminal zero-length read is not reported)"
    );
}

#[test]
fn closure_satisfies_progress_reporter_via_blanket_impl() {
    // The blanket impl lets a bare `FnMut(u64, Option<u64>)` act as a reporter.
    let src = vec![9u8; PROGRESS_CHUNK_SIZE + 1];
    let mut reader: &[u8] = &src;
    let mut sink: Vec<u8> = Vec::new();
    let mut count = 0usize;
    let mut last_seen = 0u64;
    let mut closure = |done: u64, _total: Option<u64>| {
        count += 1;
        last_seen = done;
    };

    let n = copy_with_progress(&mut reader, &mut sink, None, &mut closure).unwrap();

    assert_eq!(n, src.len() as u64);
    assert!(count >= 2, "closure reporter invoked per chunk");
    assert_eq!(last_seen, src.len() as u64);
}
