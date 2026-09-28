//! `web.inject_audio` payload resolution (inline base64 or a CAS hash, size-
//! capped). Split out of `wasm_bridge.rs`.

/// Maximum decoded inject payload — 8 MiB (PRD FND-0029). A larger clip is
/// rejected; the bound also caps in-page decode/playout CPU.
pub(crate) const MAX_INJECT_BYTES: usize = 8 * 1024 * 1024;

/// Resolve + size-bound a `web.inject_audio` payload daemon-side (Architecture §3
/// "Inject", A5, PRD D12). Exactly one of `blob_ref` / `audio_b64` must be present.
/// Returns the resolved, size-checked bytes, or `(typed_kind, message)` on
/// rejection (mapped to a typed error receipt by the caller). The shim never sees
/// the CAS — it receives raw bytes.
pub(crate) fn resolve_inject_payload(
    blob_ref: Option<&str>,
    audio_b64: Option<&str>,
    content_store: &std::sync::Arc<dyn loom_core::content_store::ContentStore>,
) -> Result<Vec<u8>, (&'static str, String)> {
    use base64::Engine as _;
    match (blob_ref, audio_b64) {
        (Some(_), Some(_)) => Err((
            "invalid_argument",
            "web.inject_audio takes exactly one of blob_ref or audio_b64, not both".to_string(),
        )),
        (None, None) => Err((
            "invalid_argument",
            "web.inject_audio requires either blob_ref or audio_b64".to_string(),
        )),
        (Some(sha), None) => {
            // Validate the ref is a real content hash BEFORE using it as a CAS
            // path component (defense-in-depth vs a traversal-shaped blob_ref).
            if sha.len() != 64
                || !sha
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err((
                    "invalid_argument",
                    "blob_ref must be a 64-char lowercase hex sha256".to_string(),
                ));
            }
            let cref = loom_core::content_store::ContentRef {
                sha256: sha.to_string(),
                size_bytes: 0, // get() re-hashes by path; size_bytes is not validated.
            };
            let bytes = content_store.get(&cref).map_err(|e| {
                (
                    "blob_not_found",
                    format!("blob_ref {sha} not resolvable: {e}"),
                )
            })?;
            if bytes.len() > MAX_INJECT_BYTES {
                return Err((
                    "payload_too_large",
                    format!(
                        "resolved audio is {} bytes, over the {MAX_INJECT_BYTES}-byte cap",
                        bytes.len()
                    ),
                ));
            }
            Ok(bytes)
        }
        (None, Some(b64)) => {
            // A5: reject BEFORE decode when the encoded length cannot fit within
            // the decoded cap — bounds decode amplification / inline-base64 DoS.
            // base64 encodes 3 bytes → 4 chars, so max encoded len = ceil(N/3)*4.
            let encoded_cap = MAX_INJECT_BYTES.div_ceil(3) * 4;
            if b64.len() > encoded_cap {
                return Err((
                    "payload_too_large",
                    format!(
                        "base64 payload is {} chars, over the {encoded_cap}-char cap \
                         (decoded max {MAX_INJECT_BYTES} bytes)",
                        b64.len()
                    ),
                ));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|_| {
                    (
                        "audio_decode_failed",
                        "audio_b64 is not valid base64".to_string(),
                    )
                })?;
            if bytes.len() > MAX_INJECT_BYTES {
                return Err((
                    "payload_too_large",
                    format!(
                        "decoded audio is {} bytes, over the {MAX_INJECT_BYTES}-byte cap",
                        bytes.len()
                    ),
                ));
            }
            Ok(bytes)
        }
    }
}

#[cfg(test)]
mod inject_payload_tests {
    use super::*;
    use base64::Engine as _;
    use loom_core::content_store::{ContentRef, ContentStore, GcReport};
    use loom_core::error::{LoomError, LoomErrorCode};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Minimal in-module ContentStore double (loom_core::mocks is feature-gated
    /// and not reachable from loom-daemon tests without extra Cargo wiring).
    struct MockStore(Mutex<HashMap<String, Vec<u8>>>);
    impl ContentStore for MockStore {
        fn put(&self, bytes: &[u8]) -> Result<ContentRef, LoomError> {
            let sha = loom_core::content_store::sha256_hex(bytes);
            self.0.lock().unwrap().insert(sha.clone(), bytes.to_vec());
            Ok(ContentRef {
                sha256: sha,
                size_bytes: bytes.len() as u64,
            })
        }
        fn get(&self, r: &ContentRef) -> Result<Vec<u8>, LoomError> {
            self.0
                .lock()
                .unwrap()
                .get(&r.sha256)
                .cloned()
                .ok_or_else(|| LoomError::new(LoomErrorCode::StoreNotFound, "not found"))
        }
        fn gc(&self, _ttl: Duration) -> Result<GcReport, LoomError> {
            Ok(GcReport {
                blobs_scanned: 0,
                blobs_collected: 0,
                bytes_freed: 0,
            })
        }
    }

    fn store() -> Arc<dyn ContentStore> {
        Arc::new(MockStore(Mutex::new(HashMap::new())))
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn valid_inline_base64_resolves_to_exact_bytes() {
        let s = store();
        let audio = b"RIFF----WAVEfake";
        let out = resolve_inject_payload(None, Some(&b64(audio)), &s).expect("resolve ok");
        assert_eq!(out, audio);
    }

    #[test]
    fn corrupt_base64_is_audio_decode_failed() {
        let s = store();
        let (kind, _) = resolve_inject_payload(None, Some("!!!not base64!!!"), &s).unwrap_err();
        assert_eq!(kind, "audio_decode_failed");
    }

    #[test]
    fn oversize_base64_rejected_before_decode() {
        let s = store();
        // A string longer than the encoded cap must be rejected WITHOUT decoding.
        let encoded_cap = MAX_INJECT_BYTES.div_ceil(3) * 4;
        let huge = "A".repeat(encoded_cap + 1);
        let (kind, _) = resolve_inject_payload(None, Some(&huge), &s).unwrap_err();
        assert_eq!(kind, "payload_too_large");
    }

    #[test]
    fn oversize_decoded_payload_rejected() {
        let s = store();
        // Encoded length within the cap, decoded length over 8 MiB.
        let big = vec![0u8; MAX_INJECT_BYTES + 1_000];
        let (kind, _) = resolve_inject_payload(None, Some(&b64(&big)), &s).unwrap_err();
        assert_eq!(kind, "payload_too_large");
    }

    #[test]
    fn neither_payload_is_invalid_argument() {
        let s = store();
        let (kind, _) = resolve_inject_payload(None, None, &s).unwrap_err();
        assert_eq!(kind, "invalid_argument");
    }

    #[test]
    fn both_payloads_is_invalid_argument() {
        let s = store();
        let (kind, _) = resolve_inject_payload(Some("a".repeat(64).as_str()), Some(&b64(b"x")), &s)
            .unwrap_err();
        assert_eq!(kind, "invalid_argument");
    }

    #[test]
    fn blob_ref_resolves_from_cas() {
        let s = store();
        let audio = b"WAVE-from-cas".to_vec();
        let cref = s.put(&audio).unwrap();
        let out = resolve_inject_payload(Some(&cref.sha256), None, &s).expect("resolve ok");
        assert_eq!(out, audio);
    }

    #[test]
    fn missing_blob_is_blob_not_found() {
        let s = store();
        let sha = "0".repeat(64);
        let (kind, _) = resolve_inject_payload(Some(&sha), None, &s).unwrap_err();
        assert_eq!(kind, "blob_not_found");
    }

    #[test]
    fn malformed_blob_ref_is_invalid_argument() {
        let s = store();
        let (kind, _) = resolve_inject_payload(Some("../../etc/passwd"), None, &s).unwrap_err();
        assert_eq!(kind, "invalid_argument");
    }
}
