//! File-based attestation schema and IO.
//!
//! `PrMetaAttestation` records a claim that PR metadata is correct
//! for a specific commit SHA. Write is atomic: temp file plus
//! rename. Read is total: missing file yields `Ok(None)`; malformed
//! or wrong-version content yields a typed error.
//!
//! Single source of truth for both the `ooda-attest` CLI and the
//! PR-side OODA binaries.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

pub const PR_META_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrMetaAttestation {
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
///
/// Generates `attested_at` via `Utc::now()`. Creates the parent
/// directory if missing. Writes to `<path>.tmp` then renames over
/// `path`; the destination is never observed in a partial state.
///
/// # Errors
///
/// Returns [`AttestError::BadShaFormat`] if `attested_sha` is not
/// 40 lowercase hex characters. Returns [`AttestError::Io`] on
/// filesystem failure. Returns [`AttestError::Parse`] if
/// serialization fails (does not occur for the well-known shape).
pub fn write_pr_meta_atomic(
    path: &Path,
    attested_sha: String,
) -> Result<PrMetaAttestation, AttestError> {
    if !is_valid_sha(&attested_sha) {
        return Err(AttestError::BadShaFormat(attested_sha));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let attestation = PrMetaAttestation {
        attested_sha,
        attested_at: Utc::now(),
        version: PR_META_SCHEMA_VERSION,
    };
    let json = serde_json::to_vec_pretty(&attestation)?;
    let tmp = tmp_path(path);
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, path)?;
    Ok(attestation)
}

/// Read the attestation at `path`.
///
/// Returns `Ok(None)` when the file does not exist. Returns
/// `Err(AttestError::SchemaVersion { .. })` when the file parses but
/// carries a different schema version. Returns
/// `Err(AttestError::BadShaFormat(_))` when the parsed SHA is not
/// 40 lowercase hex characters.
///
/// # Errors
///
/// See variants above; [`AttestError::Io`] for non-`NotFound`
/// filesystem errors, [`AttestError::Parse`] for malformed JSON.
pub fn read_pr_meta(path: &Path) -> Result<Option<PrMetaAttestation>, AttestError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AttestError::Io(e)),
    };
    let attestation: PrMetaAttestation = serde_json::from_slice(&bytes)?;
    if attestation.version != PR_META_SCHEMA_VERSION {
        return Err(AttestError::SchemaVersion {
            found: attestation.version,
            expected: PR_META_SCHEMA_VERSION,
        });
    }
    if !is_valid_sha(&attestation.attested_sha) {
        return Err(AttestError::BadShaFormat(attestation.attested_sha));
    }
    Ok(Some(attestation))
}

fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    s.into()
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
        let written = write_pr_meta_atomic(&path, VALID_SHA.to_string()).unwrap();
        let read = read_pr_meta(&path).unwrap().unwrap();
        assert_eq!(written, read);
        assert_eq!(read.attested_sha, VALID_SHA);
        assert_eq!(read.version, PR_META_SCHEMA_VERSION);
    }

    #[test]
    fn write_leaves_no_temp_file_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pr_meta_attest.json");
        write_pr_meta_atomic(&path, VALID_SHA.to_string()).unwrap();
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "temp file lingered at {tmp:?}");
    }

    #[test]
    fn read_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");
        assert!(read_pr_meta(&path).unwrap().is_none());
    }

    #[test]
    fn read_malformed_json_returns_parse_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, b"{not json").unwrap();
        match read_pr_meta(&path) {
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
        match read_pr_meta(&path) {
            Err(AttestError::SchemaVersion { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, PR_META_SCHEMA_VERSION);
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
        match read_pr_meta(&path) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "NOTHEX"),
            other => panic!("expected BadShaFormat error, got {other:?}"),
        }
    }

    #[test]
    fn write_rejects_short_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        match write_pr_meta_atomic(&path, "abc123".to_string()) {
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
        match write_pr_meta_atomic(&path, upper.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, upper),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn write_rejects_non_hex_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("attest.json");
        let bad = "g".repeat(40);
        match write_pr_meta_atomic(&path, bad.clone()) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, bad),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn write_creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("attest.json");
        assert!(!path.parent().unwrap().exists());
        write_pr_meta_atomic(&path, VALID_SHA.to_string()).unwrap();
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
}
