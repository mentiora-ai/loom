// ChromiumDownloader — pinned-URL Chromium download with sha256
// verification.
//
// # Contract semantics
// - **Supply-chain check.** sha256 mismatch returns
//   `LoomErrorCode::SupplyChainViolation { expected_hash,
//   actual_hash, url }` (mapped to `CliError::SupplyChain` by
//   `ErrorMapper`).
// - **Idempotent.** `ensure(url, expected_sha256)` skips
//   download if the binary is present AND `install_dir/.archive_sha256`
//   contains the expected SHA. The sentinel stores the archive SHA so the
//   idempotence check is semantically correct (archive SHA ≠ binary SHA).
// - **Extraction.** After SHA verify the zip archive is
//   extracted via the `zip` crate into `install_dir`. The sentinel is
//   written only after successful extraction.
// - **Internal-only.** Used by `PostinstallRunner` (download path) +
//   `DoctorRunner` (verify-only path).

use std::path::{Path, PathBuf};

use crate::CliError;

/// Outcome of a `ensure` call. `Skipped` means the binary was already
/// present + verified via sentinel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
    Skipped,
    Downloaded(PathBuf),
}

/// Configuration for the downloader.
#[derive(Debug, Clone)]
pub struct ChromiumDownloaderConfig {
    /// Install root (e.g. `~/.../chromium/`).
    pub install_dir: PathBuf,
    /// Final binary subpath relative to `install_dir`
    /// (e.g. `Chromium.app/Contents/MacOS/Chromium`).
    pub binary_subpath: PathBuf,
}

/// Stateless handle. Holds config only; curl is invoked as a subprocess.
pub struct ChromiumDownloader {
    pub(crate) config: ChromiumDownloaderConfig,
}

impl ChromiumDownloader {
    pub fn new(config: ChromiumDownloaderConfig) -> Self {
        Self { config }
    }

    /// Idempotent ensure-present: return `Skipped` when
    /// the binary exists and `install_dir/.archive_sha256` contains the
    /// expected SHA. Otherwise: download archive → verify SHA → extract
    /// zip → write sentinel.
    ///
    /// Progress-silent convenience wrapper over [`ensure_with_progress`] for
    /// callers that don't render download progress (the verify-only/doctor
    /// path never downloads, so it stays on `ensure`).
    pub async fn ensure(
        &self,
        url: &str,
        expected_sha256: &str,
    ) -> Result<DownloadOutcome, CliError> {
        let mut noop = |_: u64, _: Option<u64>| {};
        self.ensure_with_progress(url, expected_sha256, &mut noop)
            .await
    }

    /// Idempotent ensure-present with download progress (AC5).
    ///
    /// Identical to [`ensure`](Self::ensure) but invokes `reporter` once per
    /// streamed chunk during the download so the CLI can render visible
    /// progress to stderr. The archive is streamed from `curl`'s stdout
    /// through [`copy_with_progress`] into a temp file, then SHA-verified
    /// before extraction (verify-before-execute, no TOCTOU window).
    ///
    /// Robustness (PRD R5 / FND-0008):
    /// - **Bounded retry with backoff.** Transport failures (curl non-zero
    ///   exit — connection reset, timeout, a server that closes before the
    ///   advertised `Content-Length`) are retried up to
    ///   [`DOWNLOAD_MAX_ATTEMPTS`] times with exponential backoff.
    /// - **Partial-download cleanup.** The temp file is removed before every
    ///   retry and on final failure, so a truncated download never lingers or
    ///   gets extracted.
    /// - **SHA mismatch is fatal, not retried.** A corrupt/tampered archive
    ///   that nonetheless downloaded cleanly returns
    ///   [`CliError::SupplyChain`] immediately — re-fetching the same pinned
    ///   URL would only reproduce the mismatch and mask the supply-chain
    ///   signal.
    pub async fn ensure_with_progress<P>(
        &self,
        url: &str,
        expected_sha256: &str,
        reporter: &mut P,
    ) -> Result<DownloadOutcome, CliError>
    where
        P: ProgressReporter + ?Sized,
    {
        let binary = self.binary_path();
        let sentinel = sentinel_path(&self.config.install_dir);

        // Idempotence check: binary present AND sentinel matches.
        if binary.exists() && sentinel.exists() {
            let recorded = std::fs::read_to_string(&sentinel)
                .map_err(|e| CliError::Internal(format!("read sentinel: {e}")))?;
            if recorded.trim() == expected_sha256 {
                return Ok(DownloadOutcome::Skipped);
            }
        }

        // Prepare install_dir and temp download path.
        std::fs::create_dir_all(&self.config.install_dir)
            .map_err(|e| CliError::Internal(format!("create dir: {e}")))?;
        let tmp = self.config.install_dir.join(".chromium.download.tmp");

        // Download the archive into `tmp`, streaming with progress, with a
        // bounded retry + backoff over transport failures. A leftover temp
        // from a prior interrupted run (or a failed attempt) is always
        // cleaned up before re-trying so we never extract a partial file.
        let mut last_err: Option<CliError> = None;
        for attempt in 0..DOWNLOAD_MAX_ATTEMPTS {
            if attempt > 0 {
                let backoff = DOWNLOAD_BACKOFF_BASE_MS * (1u64 << (attempt - 1));
                tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
            }
            match download_streaming(url, &tmp, reporter) {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    last_err = Some(e);
                }
            }
        }
        if let Some(e) = last_err {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }

        // Verify archive SHA-256 before extraction (non-retryable on mismatch).
        let actual = sha256_of_file(&tmp).await?;
        if actual != expected_sha256 {
            let _ = std::fs::remove_file(&tmp);
            return Err(CliError::SupplyChain {
                expected_hash: expected_sha256.to_string(),
                actual_hash: actual,
                url: url.to_string(),
            });
        }

        // Extract zip archive into install_dir.
        extract_zip(&tmp, &self.config.install_dir)?;
        let _ = std::fs::remove_file(&tmp);

        // Write sentinel with archive SHA.
        std::fs::write(&sentinel, expected_sha256)
            .map_err(|e| CliError::Internal(format!("write sentinel: {e}")))?;

        Ok(DownloadOutcome::Downloaded(binary))
    }

    /// Verify-only path used by `DoctorRunner` check 4.
    /// Does NOT download; returns `Ok(())` when the binary exists and
    /// the sentinel matches `expected_sha256`.
    ///
    /// Fallback: when the literal `binary_path()` is missing but
    /// its parent directory (e.g. `Contents/MacOS/`) exists and contains at
    /// least one executable file, returns `Ok(())` without a sentinel check.
    /// This covers whole-bundle symlinks to differently-named browsers such as
    /// `Google Chrome.app`.
    pub async fn verify(&self, expected_sha256: &str) -> Result<(), CliError> {
        let binary = self.binary_path();
        if !binary.exists() {
            // Fallback: scan the parent directory for any executable.
            // Covers bundles where the browser binary name differs from the
            // pinned subpath (e.g. "Google Chrome" instead of "Chromium").
            // System installations have no .archive_sha256 sentinel, so we
            // skip the sentinel check on this path.
            if let Some(parent) = binary.parent() {
                if parent.is_dir() && find_executable_in_dir(parent)?.is_some() {
                    return Ok(());
                }
            }
            return Err(CliError::Internal(format!(
                "chromium binary not found: {} — run loom postinstall",
                binary.display()
            )));
        }
        let sentinel = sentinel_path(&self.config.install_dir);
        if !sentinel.exists() {
            return Err(CliError::Internal(format!(
                "chromium sentinel missing: {} — run loom postinstall",
                sentinel.display()
            )));
        }
        let recorded = std::fs::read_to_string(&sentinel)
            .map_err(|e| CliError::Internal(format!("read sentinel: {e}")))?;
        if recorded.trim() != expected_sha256 {
            return Err(CliError::SupplyChain {
                expected_hash: expected_sha256.to_string(),
                actual_hash: recorded.trim().to_string(),
                url: sentinel.to_string_lossy().to_string(),
            });
        }
        Ok(())
    }

    /// Returns the absolute path to the final Chromium binary. Used
    /// by `DoctorRunner` for the existence probe.
    pub fn binary_path(&self) -> PathBuf {
        self.config.install_dir.join(&self.config.binary_subpath)
    }
}

// ── Download progress reporting ──────────────────────────────────────────────
//
// The `ProgressReporter` abstraction is the minimal, transport-agnostic
// mechanism for surfacing download progress (Architecture R5 / AC5). It is a
// plain synchronous per-chunk callback — deliberately NOT an async task or
// event channel — so a download loop can invoke it inline with no extra
// machinery. `ChromiumDownloader::ensure` still downloads via `curl` today; the
// inline-postinstall slice wires an actual byte source through
// `copy_with_progress` and renders updates to stderr. This module owns only the
// read/write/report loop so it can be unit-tested with no network or daemon.

/// Synchronous per-chunk download-progress callback.
///
/// `on_progress` is invoked once after each non-empty chunk during a streaming
/// copy, receiving the cumulative bytes written so far and the total expected
/// size (`None` when the source does not advertise a length, e.g. a server that
/// omits `Content-Length`).
///
/// A blanket impl lets any `FnMut(u64, Option<u64>)` act as a reporter, so most
/// callers pass a closure (the CLI renders to stderr; tests record into a vec).
pub trait ProgressReporter {
    fn on_progress(&mut self, bytes_done: u64, total: Option<u64>);
}

impl<F: FnMut(u64, Option<u64>)> ProgressReporter for F {
    fn on_progress(&mut self, bytes_done: u64, total: Option<u64>) {
        self(bytes_done, total)
    }
}

/// Chunk size for [`copy_with_progress`] reads (64 KiB): large enough to keep
/// syscall overhead low, small enough that a multi-megabyte Chromium archive
/// yields many progress callbacks (AC5 requires ≥2 visible updates).
pub const PROGRESS_CHUNK_SIZE: usize = 64 * 1024;

/// Stream `reader` into `writer` in [`PROGRESS_CHUNK_SIZE`] chunks, invoking
/// `reporter` once after each non-empty chunk with the cumulative byte count
/// and the optional `total`. Flushes `writer` at the end and returns the total
/// number of bytes copied.
///
/// This is the transport-agnostic progress primitive: it works over any
/// `Read`/`Write` pair — an HTTP body, a `curl` stdout pipe, or an in-memory
/// buffer in tests. The download path supplies the concrete source/sink and the
/// stderr rendering; this function owns only the read/write/report loop.
///
/// The callback is not invoked for the terminal zero-length read, so an empty
/// source yields zero callbacks and returns `0`.
pub fn copy_with_progress<R, W, P>(
    reader: &mut R,
    writer: &mut W,
    total: Option<u64>,
    reporter: &mut P,
) -> std::io::Result<u64>
where
    R: std::io::Read,
    W: std::io::Write,
    P: ProgressReporter + ?Sized,
{
    let mut buf = vec![0u8; PROGRESS_CHUNK_SIZE];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        done += n as u64;
        reporter.on_progress(done, total);
    }
    writer.flush()?;
    Ok(done)
}

/// Maximum number of download attempts before [`ensure_with_progress`] gives
/// up on transport failures. The first attempt plus 2 retries = 3 tries.
pub const DOWNLOAD_MAX_ATTEMPTS: usize = 3;

/// Base backoff (ms) between download retries; doubled each subsequent retry
/// (250 ms, then 500 ms). Small enough to keep an interactive first-run snappy,
/// large enough to ride out a transient blip.
pub const DOWNLOAD_BACKOFF_BASE_MS: u64 = 250;

/// curl connect timeout (seconds) — fail fast on an unreachable host rather
/// than hanging the CLI.
const CURL_CONNECT_TIMEOUT_SECS: &str = "30";

/// curl overall transfer timeout (seconds). 30 min is generous for a
/// ~150 MB archive on a slow link while still bounding a stalled transfer.
const CURL_MAX_TIME_SECS: &str = "1800";

/// Stream `url` into `tmp` via a `curl` subprocess, reporting per-chunk
/// progress. Returns `Err` on any transport failure (spawn failure, non-zero
/// curl exit, or a write error) — the caller cleans up `tmp` and decides
/// whether to retry.
///
/// curl writes the response body to stdout (`-L` follows redirects, the
/// default https scheme provides TLS); we drain that pipe through
/// [`copy_with_progress`] so the reporter fires while bytes arrive. The pipe
/// is fully drained *before* `wait()` to avoid a buffer deadlock, and curl's
/// exit status is checked afterwards: a server that closes before sending the
/// promised `Content-Length` makes curl exit non-zero (CURLE_PARTIAL_FILE),
/// which surfaces here as a retryable error even though the read itself hit a
/// clean EOF.
fn download_streaming<P>(url: &str, tmp: &Path, reporter: &mut P) -> Result<(), CliError>
where
    P: ProgressReporter + ?Sized,
{
    use std::process::{Command, Stdio};

    let mut child = Command::new("curl")
        .args([
            "-fsSL",
            "-L",
            "--connect-timeout",
            CURL_CONNECT_TIMEOUT_SECS,
            "--max-time",
            CURL_MAX_TIME_SECS,
        ])
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| CliError::Internal(format!("spawn curl: {e}")))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::Internal("curl produced no stdout pipe".to_string()))?;
    let mut file = std::fs::File::create(tmp)
        .map_err(|e| CliError::Internal(format!("create download temp: {e}")))?;

    // Drain the pipe fully (reports progress per chunk) before reaping curl.
    let copy_res = copy_with_progress(&mut stdout, &mut file, None, reporter);
    let status = child
        .wait()
        .map_err(|e| CliError::Internal(format!("wait curl: {e}")))?;

    copy_res.map_err(|e| CliError::Internal(format!("write download temp: {e}")))?;
    if !status.success() {
        return Err(CliError::Internal(format!(
            "curl download failed for {url} (exit {})",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        )));
    }
    Ok(())
}

/// A [`ProgressReporter`] that renders coarse download progress to stderr.
///
/// Rendering is throttled: a line is emitted on the very first chunk and then
/// only after each [`STDERR_REPORT_INTERVAL_BYTES`] boundary is crossed, so a
/// large download produces a handful of `\r`-updated lines rather than
/// thousands. A final newline is emitted by [`finish`](Self::finish). Total
/// size is `None` on the streaming path (curl-to-stdout advertises no length),
/// so progress is shown as a running byte count.
pub struct StderrProgressReporter {
    label: String,
    last_reported: u64,
    emitted_any: bool,
}

/// Emit a stderr progress line each time the cumulative byte count crosses a
/// multiple of this (4 MiB).
const STDERR_REPORT_INTERVAL_BYTES: u64 = 4 * 1024 * 1024;

impl StderrProgressReporter {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            last_reported: 0,
            emitted_any: false,
        }
    }

    /// Terminate the in-place progress line with a newline if anything was
    /// rendered. Call once after the download completes.
    pub fn finish(&self) {
        if self.emitted_any {
            eprintln!();
        }
    }
}

impl ProgressReporter for StderrProgressReporter {
    fn on_progress(&mut self, bytes_done: u64, total: Option<u64>) {
        let crossed = bytes_done - self.last_reported >= STDERR_REPORT_INTERVAL_BYTES;
        if self.emitted_any && !crossed {
            return;
        }
        self.last_reported = bytes_done;
        self.emitted_any = true;
        let mib = bytes_done as f64 / (1024.0 * 1024.0);
        match total {
            Some(t) if t > 0 => {
                let pct = (bytes_done as f64 / t as f64 * 100.0).min(100.0);
                eprint!("\r{} {:.1} MiB ({:.0}%)   ", self.label, mib, pct);
            }
            _ => eprint!("\r{} {:.1} MiB   ", self.label, mib),
        }
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

/// Returns the sentinel file path for the given install directory.
/// The sentinel stores the archive SHA-256 for idempotence checking.
fn sentinel_path(install_dir: &Path) -> PathBuf {
    install_dir.join(".archive_sha256")
}

/// Scan `dir` for the first executable regular file (follows symlinks).
/// Returns `Some(path)` when found, `None` when the directory contains no
/// executable files. Returns `Err` only when the directory itself cannot be
/// read.
///
/// On Unix, a file is considered executable when any of the owner/group/other
/// execute bits are set (`mode & 0o111 != 0`). On non-Unix platforms every
/// regular file is considered executable.
fn find_executable_in_dir(dir: &Path) -> Result<Option<PathBuf>, CliError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| CliError::Internal(format!("read dir {}: {e}", dir.display())))?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        // Follow symlinks so a dangling symlink is safely skipped.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if meta.mode() & 0o111 != 0 {
                return Ok(Some(path));
            }
        }
        #[cfg(not(unix))]
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Extract a zip archive into `dest_dir`. Each entry is written relative
/// to `dest_dir`. Unix executable permissions are restored when available.
fn extract_zip(archive: &Path, dest_dir: &Path) -> Result<(), CliError> {
    let file = std::fs::File::open(archive)
        .map_err(|e| CliError::Internal(format!("open archive: {e}")))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| CliError::Internal(format!("parse zip: {e}")))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| CliError::Internal(format!("zip entry {i}: {e}")))?;
        // zip 9 made `name()` fallible: an entry name need not be valid UTF-8.
        let name = entry
            .name()
            .map_err(|e| CliError::Internal(format!("zip entry {i} name: {e}")))?
            .into_owned();

        // Confine every entry to `dest_dir`. A zip entry may name `..` or an
        // absolute path and so write outside the destination — the hazard behind
        // zip's own `extract()` advisories (RUSTSEC-2026-0367/0368/0369). This
        // function does its own extraction, so the guard has to be ours. The
        // archive is SHA-verified before we get here, so this is defence in
        // depth rather than a live hole, but the check is free.
        let relative = std::path::Path::new(&name);
        if relative.components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        }) {
            return Err(CliError::Internal(format!(
                "refusing zip entry {i} with a non-relative path: {name}"
            )));
        }
        let entry_path = dest_dir.join(relative);

        if entry.is_dir() {
            std::fs::create_dir_all(&entry_path).map_err(|e| {
                CliError::Internal(format!("create dir {}: {e}", entry_path.display()))
            })?;
        } else {
            if let Some(parent) = entry_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    CliError::Internal(format!("create dir {}: {e}", parent.display()))
                })?;
            }
            let mut out = std::fs::File::create(&entry_path)
                .map_err(|e| CliError::Internal(format!("create {}: {e}", entry_path.display())))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| {
                CliError::Internal(format!("extract {}: {e}", entry_path.display()))
            })?;

            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&entry_path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| {
                        CliError::Internal(format!("chmod {}: {e}", entry_path.display()))
                    })?;
            }
        }
    }
    Ok(())
}

/// Pure helper: compute sha256 of a file path. Delegates to `shasum`
/// (macOS) or `sha256sum` (Linux). Returns lowercase hex.
pub async fn sha256_of_file(path: &Path) -> Result<String, CliError> {
    let output = std::process::Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .or_else(|_| std::process::Command::new("sha256sum").arg(path).output())
        .map_err(|e| CliError::Internal(format!("sha256 command: {e}")))?;
    if !output.status.success() {
        return Err(CliError::Internal(format!(
            "sha256 command failed for {}",
            path.display()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.split_whitespace().next().unwrap_or("").to_string())
}
