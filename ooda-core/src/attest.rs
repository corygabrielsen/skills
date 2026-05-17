//! File-based attestation schema and IO.
//!
//! An attestation is a signed claim that a particular axis of work
//! is correct at a specific commit SHA. Each axis has its own
//! attestation type and on-disk file; readers and writers across
//! producer (attest CLI) and consumer (OODA decide layer) share
//! this module as the single schema definition.
//!
//! # Invariants
//!
//! - **Atomic write**: a partial write is never observed by readers
//!   (via [`crate::atomic_io::write_atomic`]).
//! - **Total read**: a missing file is `Ok(None)`; malformed content
//!   and wrong-schema content yield typed errors distinguishable
//!   from genuine IO failure.
//! - **SHA discipline**: 40 lowercase hex characters at both write
//!   and read; any other shape is rejected at the type boundary.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

pub const PULL_REQUEST_METADATA_SCHEMA_VERSION: u32 = 1;
pub const DOC_REVIEW_SCHEMA_VERSION: u32 = 1;
pub const CLAUDE_REVIEW_SCHEMA_VERSION: u32 = 1;
pub const CLOSEOUT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestMetadataAttestation {
    pub attested_sha: String,
    pub attested_at: DateTime<Utc>,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocReviewAttestation {
    pub attested_sha: String,
    pub attested_at: DateTime<Utc>,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeReviewAttestation {
    pub attested_sha: String,
    pub attested_at: DateTime<Utc>,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseoutAttestation {
    pub attested_sha: String,
    pub attested_at: DateTime<Utc>,
    pub version: u32,
}

#[derive(Debug)]
pub enum AttestError {
    Io(io::Error),
    Parse(serde_json::Error),
    SchemaVersion { found: u32, expected: u32 },
    BadShaFormat(String),
}

impl std::fmt::Display for AttestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "attestation io error: {e}"),
            Self::Parse(e) => write!(f, "attestation parse error: {e}"),
            Self::SchemaVersion { found, expected } => write!(
                f,
                "attestation schema version mismatch: found {found}, expected {expected}"
            ),
            Self::BadShaFormat(s) => write!(
                f,
                "attestation sha must be 40 lowercase hex characters: {s:?}"
            ),
        }
    }
}

impl std::error::Error for AttestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::SchemaVersion { .. } | Self::BadShaFormat(_) => None,
        }
    }
}

impl From<io::Error> for AttestError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for AttestError {
    fn from(e: serde_json::Error) -> Self {
        Self::Parse(e)
    }
}

fn is_valid_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Atomically write an attestation for `attested_sha` to `path`.
/// Stamps `attested_at` from the system clock; creates parent
/// directories on demand; preserves atomicity per
/// [`crate::atomic_io`].
///
/// # Errors
///
/// - [`AttestError::BadShaFormat`] — `attested_sha` violates the
///   40-lowercase-hex discipline.
/// - [`AttestError::Io`] — filesystem failure.
/// - [`AttestError::Parse`] — serialization failure (unreachable
///   for the well-known shape).
pub fn write_pull_request_metadata_atomic(
    path: &Path,
    attested_sha: String,
) -> Result<PullRequestMetadataAttestation, AttestError> {
    if !is_valid_sha(&attested_sha) {
        return Err(AttestError::BadShaFormat(attested_sha));
    }
    let attestation = PullRequestMetadataAttestation {
        attested_sha,
        attested_at: Utc::now(),
        version: PULL_REQUEST_METADATA_SCHEMA_VERSION,
    };
    let json = serde_json::to_vec_pretty(&attestation)?;
    crate::atomic_io::write_atomic(path, &json)?;
    Ok(attestation)
}

/// Read the attestation at `path`. Total over the absence case
/// (missing file ⇒ `Ok(None)`).
///
/// # Errors
///
/// - [`AttestError::Io`] — non-`NotFound` filesystem failure.
/// - [`AttestError::Parse`] — malformed JSON.
/// - [`AttestError::SchemaVersion`] — parsed cleanly under a
///   different schema version.
/// - [`AttestError::BadShaFormat`] — parsed value violates the
///   40-lowercase-hex discipline.
pub fn read_pull_request_metadata(
    path: &Path,
) -> Result<Option<PullRequestMetadataAttestation>, AttestError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AttestError::Io(e)),
    };
    let attestation: PullRequestMetadataAttestation = serde_json::from_slice(&bytes)?;
    if attestation.version != PULL_REQUEST_METADATA_SCHEMA_VERSION {
        return Err(AttestError::SchemaVersion {
            found: attestation.version,
            expected: PULL_REQUEST_METADATA_SCHEMA_VERSION,
        });
    }
    if !is_valid_sha(&attestation.attested_sha) {
        return Err(AttestError::BadShaFormat(attestation.attested_sha));
    }
    Ok(Some(attestation))
}

/// Atomically write a doc-review attestation. Mirrors
/// [`write_pull_request_metadata_atomic`] — same invariants, same
/// error taxonomy.
///
/// # Errors
///
/// See [`write_pull_request_metadata_atomic`].
pub fn write_doc_review_atomic(
    path: &Path,
    attested_sha: String,
) -> Result<DocReviewAttestation, AttestError> {
    if !is_valid_sha(&attested_sha) {
        return Err(AttestError::BadShaFormat(attested_sha));
    }
    let attestation = DocReviewAttestation {
        attested_sha,
        attested_at: Utc::now(),
        version: DOC_REVIEW_SCHEMA_VERSION,
    };
    let json = serde_json::to_vec_pretty(&attestation)?;
    crate::atomic_io::write_atomic(path, &json)?;
    Ok(attestation)
}

/// Read the doc-review attestation at `path`. Mirrors
/// [`read_pull_request_metadata`].
///
/// # Errors
///
/// See [`read_pull_request_metadata`].
pub fn read_doc_review(path: &Path) -> Result<Option<DocReviewAttestation>, AttestError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AttestError::Io(e)),
    };
    let attestation: DocReviewAttestation = serde_json::from_slice(&bytes)?;
    if attestation.version != DOC_REVIEW_SCHEMA_VERSION {
        return Err(AttestError::SchemaVersion {
            found: attestation.version,
            expected: DOC_REVIEW_SCHEMA_VERSION,
        });
    }
    if !is_valid_sha(&attestation.attested_sha) {
        return Err(AttestError::BadShaFormat(attestation.attested_sha));
    }
    Ok(Some(attestation))
}

/// Atomically write a Claude-review attestation. Mirrors
/// [`write_pull_request_metadata_atomic`].
///
/// # Errors
///
/// See [`write_pull_request_metadata_atomic`].
pub fn write_claude_review_atomic(
    path: &Path,
    attested_sha: String,
) -> Result<ClaudeReviewAttestation, AttestError> {
    if !is_valid_sha(&attested_sha) {
        return Err(AttestError::BadShaFormat(attested_sha));
    }
    let attestation = ClaudeReviewAttestation {
        attested_sha,
        attested_at: Utc::now(),
        version: CLAUDE_REVIEW_SCHEMA_VERSION,
    };
    let json = serde_json::to_vec_pretty(&attestation)?;
    crate::atomic_io::write_atomic(path, &json)?;
    Ok(attestation)
}

/// Read the Claude-review attestation at `path`. Mirrors
/// [`read_pull_request_metadata`].
///
/// # Errors
///
/// See [`read_pull_request_metadata`].
pub fn read_claude_review(path: &Path) -> Result<Option<ClaudeReviewAttestation>, AttestError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AttestError::Io(e)),
    };
    let attestation: ClaudeReviewAttestation = serde_json::from_slice(&bytes)?;
    if attestation.version != CLAUDE_REVIEW_SCHEMA_VERSION {
        return Err(AttestError::SchemaVersion {
            found: attestation.version,
            expected: CLAUDE_REVIEW_SCHEMA_VERSION,
        });
    }
    if !is_valid_sha(&attestation.attested_sha) {
        return Err(AttestError::BadShaFormat(attestation.attested_sha));
    }
    Ok(Some(attestation))
}

/// Atomically write a closeout attestation. Mirrors
/// [`write_pull_request_metadata_atomic`].
///
/// # Errors
///
/// See [`write_pull_request_metadata_atomic`].
pub fn write_closeout_atomic(
    path: &Path,
    attested_sha: String,
) -> Result<CloseoutAttestation, AttestError> {
    if !is_valid_sha(&attested_sha) {
        return Err(AttestError::BadShaFormat(attested_sha));
    }
    let attestation = CloseoutAttestation {
        attested_sha,
        attested_at: Utc::now(),
        version: CLOSEOUT_SCHEMA_VERSION,
    };
    let json = serde_json::to_vec_pretty(&attestation)?;
    crate::atomic_io::write_atomic(path, &json)?;
    Ok(attestation)
}

/// Read the closeout attestation at `path`. Mirrors
/// [`read_pull_request_metadata`].
///
/// # Errors
///
/// See [`read_pull_request_metadata`].
pub fn read_closeout(path: &Path) -> Result<Option<CloseoutAttestation>, AttestError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AttestError::Io(e)),
    };
    let attestation: CloseoutAttestation = serde_json::from_slice(&bytes)?;
    if attestation.version != CLOSEOUT_SCHEMA_VERSION {
        return Err(AttestError::SchemaVersion {
            found: attestation.version,
            expected: CLOSEOUT_SCHEMA_VERSION,
        });
    }
    if !is_valid_sha(&attestation.attested_sha) {
        return Err(AttestError::BadShaFormat(attestation.attested_sha));
    }
    Ok(Some(attestation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const VALID_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn round_trip_write_then_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pr_meta_attest.json");
        let written = write_pull_request_metadata_atomic(&path, VALID_SHA.to_string()).unwrap();
        let read = read_pull_request_metadata(&path).unwrap().unwrap();
        assert_eq!(written, read);
        assert_eq!(read.attested_sha, VALID_SHA);
        assert_eq!(read.version, PULL_REQUEST_METADATA_SCHEMA_VERSION);
    }

    #[test]
    fn write_leaves_no_temp_file_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pr_meta_attest.json");
        write_pull_request_metadata_atomic(&path, VALID_SHA.to_string()).unwrap();
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "temp file lingered at {tmp:?}");
    }

    #[test]
    fn read_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");
        assert!(read_pull_request_metadata(&path).unwrap().is_none());
    }

    #[test]
    fn read_malformed_json_returns_parse_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, b"{not json").unwrap();
        match read_pull_request_metadata(&path) {
            Err(AttestError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn read_wrong_schema_version_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vmismatch.json");
        let body = format!(
            r#"{{"attested_sha":"{VALID_SHA}","attested_at":"2026-05-16T12:34:56Z","version":99}}"#
        );
        fs::write(&path, body).unwrap();
        match read_pull_request_metadata(&path) {
            Err(AttestError::SchemaVersion { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, PULL_REQUEST_METADATA_SCHEMA_VERSION);
            }
            other => panic!("expected SchemaVersion error, got {other:?}"),
        }
    }

    #[test]
    fn read_invalid_sha_format_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("badsha.json");
        let body = r#"{"attested_sha":"NOTHEX","attested_at":"2026-05-16T12:34:56Z","version":1}"#;
        fs::write(&path, body).unwrap();
        match read_pull_request_metadata(&path) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "NOTHEX"),
            other => panic!("expected BadShaFormat error, got {other:?}"),
        }
    }

    #[test]
    fn write_rejects_short_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        match write_pull_request_metadata_atomic(&path, "abc123".to_string()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "abc123"),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn write_rejects_uppercase_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        let upper = VALID_SHA.to_uppercase();
        match write_pull_request_metadata_atomic(&path, upper.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, upper),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn write_rejects_non_hex_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        let bad = "g".repeat(40);
        match write_pull_request_metadata_atomic(&path, bad.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, bad),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn write_creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("attest.json");
        assert!(!path.parent().unwrap().exists());
        write_pull_request_metadata_atomic(&path, VALID_SHA.to_string()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn display_renders_each_variant() {
        let io_err = AttestError::Io(io::Error::other("boom"));
        assert!(format!("{io_err}").contains("io error"));
        let ver = AttestError::SchemaVersion {
            found: 2,
            expected: 1,
        };
        assert!(format!("{ver}").contains("schema version mismatch"));
        let sha = AttestError::BadShaFormat("nope".to_string());
        assert!(format!("{sha}").contains("40 lowercase hex"));
    }

    // ── DocReviewAttestation mirror ──

    #[test]
    fn doc_review_round_trip_write_then_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc_review_attest.json");
        let written = write_doc_review_atomic(&path, VALID_SHA.to_string()).unwrap();
        let read = read_doc_review(&path).unwrap().unwrap();
        assert_eq!(written, read);
        assert_eq!(read.attested_sha, VALID_SHA);
        assert_eq!(read.version, DOC_REVIEW_SCHEMA_VERSION);
    }

    #[test]
    fn doc_review_write_leaves_no_temp_file_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("doc_review_attest.json");
        write_doc_review_atomic(&path, VALID_SHA.to_string()).unwrap();
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "temp file lingered at {tmp:?}");
    }

    #[test]
    fn doc_review_read_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");
        assert!(read_doc_review(&path).unwrap().is_none());
    }

    #[test]
    fn doc_review_read_malformed_json_returns_parse_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, b"{not json").unwrap();
        match read_doc_review(&path) {
            Err(AttestError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn doc_review_read_wrong_schema_version_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vmismatch.json");
        let body = format!(
            r#"{{"attested_sha":"{VALID_SHA}","attested_at":"2026-05-16T12:34:56Z","version":99}}"#
        );
        fs::write(&path, body).unwrap();
        match read_doc_review(&path) {
            Err(AttestError::SchemaVersion { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, DOC_REVIEW_SCHEMA_VERSION);
            }
            other => panic!("expected SchemaVersion error, got {other:?}"),
        }
    }

    #[test]
    fn doc_review_read_invalid_sha_format_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("badsha.json");
        let body = r#"{"attested_sha":"NOTHEX","attested_at":"2026-05-16T12:34:56Z","version":1}"#;
        fs::write(&path, body).unwrap();
        match read_doc_review(&path) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "NOTHEX"),
            other => panic!("expected BadShaFormat error, got {other:?}"),
        }
    }

    #[test]
    fn doc_review_write_rejects_short_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        match write_doc_review_atomic(&path, "abc123".to_string()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "abc123"),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn doc_review_write_rejects_uppercase_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        let upper = VALID_SHA.to_uppercase();
        match write_doc_review_atomic(&path, upper.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, upper),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn doc_review_write_rejects_non_hex_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        let bad = "g".repeat(40);
        match write_doc_review_atomic(&path, bad.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, bad),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn doc_review_write_creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("attest.json");
        assert!(!path.parent().unwrap().exists());
        write_doc_review_atomic(&path, VALID_SHA.to_string()).unwrap();
        assert!(path.exists());
    }

    // ── ClaudeReviewAttestation mirror ──

    #[test]
    fn claude_review_round_trip_write_then_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("claude_review_attest.json");
        let written = write_claude_review_atomic(&path, VALID_SHA.to_string()).unwrap();
        let read = read_claude_review(&path).unwrap().unwrap();
        assert_eq!(written, read);
        assert_eq!(read.attested_sha, VALID_SHA);
        assert_eq!(read.version, CLAUDE_REVIEW_SCHEMA_VERSION);
    }

    #[test]
    fn claude_review_write_leaves_no_temp_file_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("claude_review_attest.json");
        write_claude_review_atomic(&path, VALID_SHA.to_string()).unwrap();
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "temp file lingered at {tmp:?}");
    }

    #[test]
    fn claude_review_read_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");
        assert!(read_claude_review(&path).unwrap().is_none());
    }

    #[test]
    fn claude_review_read_malformed_json_returns_parse_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, b"{not json").unwrap();
        match read_claude_review(&path) {
            Err(AttestError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn claude_review_read_wrong_schema_version_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vmismatch.json");
        let body = format!(
            r#"{{"attested_sha":"{VALID_SHA}","attested_at":"2026-05-16T12:34:56Z","version":99}}"#
        );
        fs::write(&path, body).unwrap();
        match read_claude_review(&path) {
            Err(AttestError::SchemaVersion { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, CLAUDE_REVIEW_SCHEMA_VERSION);
            }
            other => panic!("expected SchemaVersion error, got {other:?}"),
        }
    }

    #[test]
    fn claude_review_read_invalid_sha_format_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("badsha.json");
        let body = r#"{"attested_sha":"NOTHEX","attested_at":"2026-05-16T12:34:56Z","version":1}"#;
        fs::write(&path, body).unwrap();
        match read_claude_review(&path) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "NOTHEX"),
            other => panic!("expected BadShaFormat error, got {other:?}"),
        }
    }

    #[test]
    fn claude_review_write_rejects_short_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        match write_claude_review_atomic(&path, "abc123".to_string()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "abc123"),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn claude_review_write_rejects_uppercase_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        let upper = VALID_SHA.to_uppercase();
        match write_claude_review_atomic(&path, upper.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, upper),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn claude_review_write_rejects_non_hex_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        let bad = "g".repeat(40);
        match write_claude_review_atomic(&path, bad.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, bad),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn claude_review_write_creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("attest.json");
        assert!(!path.parent().unwrap().exists());
        write_claude_review_atomic(&path, VALID_SHA.to_string()).unwrap();
        assert!(path.exists());
    }

    // ── CloseoutAttestation mirror ──

    #[test]
    fn closeout_round_trip_write_then_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("closeout_attest.json");
        let written = write_closeout_atomic(&path, VALID_SHA.to_string()).unwrap();
        let read = read_closeout(&path).unwrap().unwrap();
        assert_eq!(written, read);
        assert_eq!(read.attested_sha, VALID_SHA);
        assert_eq!(read.version, CLOSEOUT_SCHEMA_VERSION);
    }

    #[test]
    fn closeout_write_leaves_no_temp_file_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("closeout_attest.json");
        write_closeout_atomic(&path, VALID_SHA.to_string()).unwrap();
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "temp file lingered at {tmp:?}");
    }

    #[test]
    fn closeout_read_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");
        assert!(read_closeout(&path).unwrap().is_none());
    }

    #[test]
    fn closeout_read_malformed_json_returns_parse_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, b"{not json").unwrap();
        match read_closeout(&path) {
            Err(AttestError::Parse(_)) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn closeout_read_wrong_schema_version_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vmismatch.json");
        let body = format!(
            r#"{{"attested_sha":"{VALID_SHA}","attested_at":"2026-05-16T12:34:56Z","version":99}}"#
        );
        fs::write(&path, body).unwrap();
        match read_closeout(&path) {
            Err(AttestError::SchemaVersion { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, CLOSEOUT_SCHEMA_VERSION);
            }
            other => panic!("expected SchemaVersion error, got {other:?}"),
        }
    }

    #[test]
    fn closeout_read_invalid_sha_format_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("badsha.json");
        let body = r#"{"attested_sha":"NOTHEX","attested_at":"2026-05-16T12:34:56Z","version":1}"#;
        fs::write(&path, body).unwrap();
        match read_closeout(&path) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "NOTHEX"),
            other => panic!("expected BadShaFormat error, got {other:?}"),
        }
    }

    #[test]
    fn closeout_write_rejects_short_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        match write_closeout_atomic(&path, "abc123".to_string()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "abc123"),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn closeout_write_rejects_uppercase_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        let upper = VALID_SHA.to_uppercase();
        match write_closeout_atomic(&path, upper.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, upper),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn closeout_write_rejects_non_hex_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        let bad = "g".repeat(40);
        match write_closeout_atomic(&path, bad.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, bad),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn closeout_write_creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("attest.json");
        assert!(!path.parent().unwrap().exists());
        write_closeout_atomic(&path, VALID_SHA.to_string()).unwrap();
        assert!(path.exists());
    }
}
