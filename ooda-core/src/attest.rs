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
//! - **Cross-process write serialisation**: every `write_*_atomic`
//!   acquires a [`crate::file_lock::FileLock`] on a sidecar for the
//!   duration of the write. Two concurrent attest invocations against
//!   the same path serialise; the loser observes the winner's bytes
//!   on its next read. Read-then-decide-then-write callers that need
//!   the read tied to the write should take a [`crate::file_lock::FileLock`] over the
//!   full RMW window externally — the per-write lock alone does not
//!   close that gap.
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
pub const REVIEW_CLASS_SCHEMA_VERSION: u32 = 1;

/// Upper bound on a review-class name. Names render on one prompt
/// line and one dashboard line; anything longer is a body, not a
/// name.
pub const REVIEW_CLASS_NAME_MAX_BYTES: usize = 200;

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

/// One `path:line` location an issue class was fixed or judged at.
///
/// `path` is repo-relative in canonical form: non-empty, no leading
/// `/`, no `\`, no ASCII control bytes. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReviewSite {
    pub path: String,
    pub line: u32,
}

impl ReviewSite {
    /// Parse the `path:line` surface form. The line is the decimal
    /// run after the last `:`, so a path containing `:` still parses.
    ///
    /// # Errors
    ///
    /// Returns [`AttestError::InvalidReviewClass`] when the input
    /// lacks a `:`, the line is not a positive integer, or the path
    /// violates the canonical-form rules.
    pub fn parse(s: &str) -> Result<Self, AttestError> {
        let s = s.trim();
        let Some((path, line)) = s.rsplit_once(':') else {
            return Err(AttestError::InvalidReviewClass(format!(
                "site {s:?} is not of the form path:line"
            )));
        };
        let line: u32 = line.parse().map_err(|_| {
            AttestError::InvalidReviewClass(format!("site {s:?}: line is not an integer"))
        })?;
        let site = Self {
            path: path.to_owned(),
            line,
        };
        site.validate()?;
        Ok(site)
    }

    fn validate(&self) -> Result<(), AttestError> {
        let p = &self.path;
        let reason = if p.is_empty() {
            Some("path is empty")
        } else if p.starts_with('/') {
            Some("path has a leading '/'")
        } else if p.contains('\\') {
            Some("path contains '\\'")
        } else if p.bytes().any(|b| b < 0x20 || b == 0x7f) {
            Some("path contains an ASCII control byte")
        } else if self.line == 0 {
            Some("line must be >= 1")
        } else {
            None
        };
        match reason {
            Some(r) => Err(AttestError::InvalidReviewClass(format!(
                "site {}:{}: {r}",
                self.path, self.line
            ))),
            None => Ok(()),
        }
    }
}

impl std::fmt::Display for ReviewSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.path, self.line)
    }
}

/// One issue class the reviewers raised, with every site the agent
/// fixed or judged for it. A class with no sites is unrepresentable
/// on disk: the witness list is the evidence the attestation exists
/// to carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewClassEntry {
    pub class: String,
    pub sites: Vec<ReviewSite>,
}

impl ReviewClassEntry {
    fn validate(&self) -> Result<(), AttestError> {
        let name = &self.class;
        if name.trim().is_empty() {
            return Err(AttestError::InvalidReviewClass(
                "class name is empty".to_string(),
            ));
        }
        if name.len() > REVIEW_CLASS_NAME_MAX_BYTES {
            return Err(AttestError::InvalidReviewClass(format!(
                "class name exceeds {REVIEW_CLASS_NAME_MAX_BYTES} bytes"
            )));
        }
        if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(AttestError::InvalidReviewClass(format!(
                "class name {name:?} contains an ASCII control byte"
            )));
        }
        if self.sites.is_empty() {
            return Err(AttestError::InvalidReviewClass(format!(
                "class {name:?} has no sites"
            )));
        }
        for site in &self.sites {
            site.validate()?;
        }
        Ok(())
    }
}

/// Attestation that every issue class raised by review threads has
/// been swept across the working tree. Content-keyed: the consumer
/// compares `attested_at` against thread creation times, not
/// `attested_sha` against HEAD. `classes` is non-empty and every
/// entry carries at least one site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewClassAttestation {
    pub attested_sha: String,
    pub attested_at: DateTime<Utc>,
    pub version: u32,
    pub classes: Vec<ReviewClassEntry>,
}

/// Validate a class list against the on-disk invariants. Shared by
/// the writer (before serialising) and the reader (after parsing),
/// so a hand-edited file cannot smuggle an empty witness list past
/// the consumer.
///
/// # Errors
///
/// Returns [`AttestError::InvalidReviewClass`] naming the first
/// violated rule.
pub fn validate_review_classes(classes: &[ReviewClassEntry]) -> Result<(), AttestError> {
    if classes.is_empty() {
        return Err(AttestError::InvalidReviewClass(
            "at least one class is required".to_string(),
        ));
    }
    for entry in classes {
        entry.validate()?;
    }
    Ok(())
}

#[derive(Debug)]
pub enum AttestError {
    Io(io::Error),
    Parse(serde_json::Error),
    SchemaVersion {
        found: u32,
        expected: u32,
    },
    BadShaFormat(String),
    /// A review-class payload violated the witness invariants (empty
    /// class list, empty class name, class without sites, malformed
    /// site).
    InvalidReviewClass(String),
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
            Self::InvalidReviewClass(s) => write!(f, "invalid review-class attestation: {s}"),
        }
    }
}

impl std::error::Error for AttestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::SchemaVersion { .. } | Self::BadShaFormat(_) | Self::InvalidReviewClass(_) => {
                None
            }
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = crate::file_lock::FileLock::acquire(path)?;
    crate::atomic_io::write_atomic(path, &json)?;
    Ok(attestation)
}

/// Acquire the cross-process advisory lock that guards every
/// `write_*_atomic` in this module. Callers performing a
/// read → decide → write sequence on the same attestation path
/// should hold this guard across the whole window so a concurrent
/// invocation does not slip in a write between their read and
/// their decision.
///
/// # Errors
///
/// Propagates [`crate::file_lock::FileLock::acquire`] failures.
pub fn attest_lock(path: &Path) -> std::io::Result<crate::file_lock::FileLock> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::file_lock::FileLock::acquire(path)
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = crate::file_lock::FileLock::acquire(path)?;
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = crate::file_lock::FileLock::acquire(path)?;
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = crate::file_lock::FileLock::acquire(path)?;
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

/// Atomically write a review-class attestation. Same write protocol
/// as the sibling axes plus the witness-list validation.
///
/// # Errors
///
/// - [`AttestError::BadShaFormat`] — `attested_sha` violates the
///   40-lowercase-hex discipline.
/// - [`AttestError::InvalidReviewClass`] — `classes` violates the
///   witness invariants (see [`validate_review_classes`]).
/// - [`AttestError::Io`] / [`AttestError::Parse`] — as the siblings.
pub fn write_review_class_atomic(
    path: &Path,
    attested_sha: String,
    classes: Vec<ReviewClassEntry>,
) -> Result<ReviewClassAttestation, AttestError> {
    if !is_valid_sha(&attested_sha) {
        return Err(AttestError::BadShaFormat(attested_sha));
    }
    validate_review_classes(&classes)?;
    let attestation = ReviewClassAttestation {
        attested_sha,
        attested_at: Utc::now(),
        version: REVIEW_CLASS_SCHEMA_VERSION,
        classes,
    };
    let json = serde_json::to_vec_pretty(&attestation)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = crate::file_lock::FileLock::acquire(path)?;
    crate::atomic_io::write_atomic(path, &json)?;
    Ok(attestation)
}

/// Read the review-class attestation at `path`. Mirrors
/// [`read_pull_request_metadata`] and additionally re-validates the
/// witness list, so a file that parses but carries no sites is
/// rejected the same way a malformed one is.
///
/// # Errors
///
/// See [`read_pull_request_metadata`], plus
/// [`AttestError::InvalidReviewClass`] for a witness-list violation.
pub fn read_review_class(path: &Path) -> Result<Option<ReviewClassAttestation>, AttestError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AttestError::Io(e)),
    };
    let attestation: ReviewClassAttestation = serde_json::from_slice(&bytes)?;
    if attestation.version != REVIEW_CLASS_SCHEMA_VERSION {
        return Err(AttestError::SchemaVersion {
            found: attestation.version,
            expected: REVIEW_CLASS_SCHEMA_VERSION,
        });
    }
    if !is_valid_sha(&attestation.attested_sha) {
        return Err(AttestError::BadShaFormat(attestation.attested_sha));
    }
    validate_review_classes(&attestation.classes)?;
    Ok(Some(attestation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const VALID_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn site(path: &str, line: u32) -> ReviewSite {
        ReviewSite {
            path: path.to_string(),
            line,
        }
    }

    fn entry(class: &str, sites: &[(&str, u32)]) -> ReviewClassEntry {
        ReviewClassEntry {
            class: class.to_string(),
            sites: sites.iter().map(|(p, l)| site(p, *l)).collect(),
        }
    }

    // ── ReviewClassAttestation ──

    #[test]
    fn review_site_parses_path_line_form() {
        let s = ReviewSite::parse("src/lib.rs:42").unwrap();
        assert_eq!(s.path, "src/lib.rs");
        assert_eq!(s.line, 42);
        assert_eq!(s.to_string(), "src/lib.rs:42");
    }

    #[test]
    fn review_site_splits_on_last_colon() {
        let s = ReviewSite::parse("weird:name.rs:7").unwrap();
        assert_eq!(s.path, "weird:name.rs");
        assert_eq!(s.line, 7);
    }

    #[test]
    fn review_site_rejects_malformed_inputs() {
        for bad in [
            "src/lib.rs",
            "src/lib.rs:",
            ":42",
            "src/lib.rs:zero",
            "src/lib.rs:0",
            "/abs/path.rs:1",
            "a\\b.rs:1",
            "a\nb.rs:1",
        ] {
            match ReviewSite::parse(bad) {
                Err(AttestError::InvalidReviewClass(_)) => {}
                other => panic!("{bad:?}: expected InvalidReviewClass, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_review_classes_rejects_empty_list() {
        match validate_review_classes(&[]) {
            Err(AttestError::InvalidReviewClass(m)) => assert!(m.contains("at least one")),
            other => panic!("expected InvalidReviewClass, got {other:?}"),
        }
    }

    #[test]
    fn validate_review_classes_rejects_class_without_sites() {
        let e = ReviewClassEntry {
            class: "unwrap in library code".into(),
            sites: vec![],
        };
        match validate_review_classes(std::slice::from_ref(&e)) {
            Err(AttestError::InvalidReviewClass(m)) => assert!(m.contains("no sites")),
            other => panic!("expected InvalidReviewClass, got {other:?}"),
        }
    }

    #[test]
    fn validate_review_classes_rejects_blank_or_control_byte_names() {
        for name in ["", "   ", "a\nb"] {
            let e = ReviewClassEntry {
                class: name.into(),
                sites: vec![site("src/a.rs", 1)],
            };
            assert!(
                matches!(
                    validate_review_classes(std::slice::from_ref(&e)),
                    Err(AttestError::InvalidReviewClass(_))
                ),
                "{name:?} must be rejected",
            );
        }
        let long = ReviewClassEntry {
            class: "x".repeat(REVIEW_CLASS_NAME_MAX_BYTES + 1),
            sites: vec![site("src/a.rs", 1)],
        };
        assert!(matches!(
            validate_review_classes(std::slice::from_ref(&long)),
            Err(AttestError::InvalidReviewClass(_))
        ));
    }

    #[test]
    fn review_class_round_trip_write_then_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("review_class_attest.json");
        let classes = vec![
            entry(
                "unwrap in library code",
                &[("src/a.rs", 12), ("src/b.rs", 40)],
            ),
            entry("missing error context", &[("src/c.rs", 3)]),
        ];
        let written = write_review_class_atomic(&path, VALID_SHA.to_string(), classes).unwrap();
        let read = read_review_class(&path).unwrap().unwrap();
        assert_eq!(written, read);
        assert_eq!(read.version, REVIEW_CLASS_SCHEMA_VERSION);
        assert_eq!(read.classes.len(), 2);
        assert_eq!(read.classes[0].sites[1].to_string(), "src/b.rs:40");
    }

    #[test]
    fn review_class_write_rejects_empty_classes_and_writes_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("review_class_attest.json");
        match write_review_class_atomic(&path, VALID_SHA.to_string(), vec![]) {
            Err(AttestError::InvalidReviewClass(_)) => {}
            other => panic!("expected InvalidReviewClass, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn review_class_write_rejects_short_sha() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("review_class_attest.json");
        let classes = vec![entry("c", &[("src/a.rs", 1)])];
        match write_review_class_atomic(&path, "abc".to_string(), classes) {
            Err(AttestError::BadShaFormat(s)) => assert_eq!(s, "abc"),
            other => panic!("expected BadShaFormat, got {other:?}"),
        }
    }

    #[test]
    fn review_class_read_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        assert!(
            read_review_class(&dir.path().join("nope.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn review_class_read_rejects_hand_edited_empty_sites() {
        // A file that parses cleanly but carries a class with no
        // sites must fail the read: the witness list is the point.
        let dir = tempdir().unwrap();
        let path = dir.path().join("review_class_attest.json");
        let body = format!(
            r#"{{"attested_sha":"{VALID_SHA}","attested_at":"2026-05-16T12:34:56Z","version":1,"classes":[{{"class":"x","sites":[]}}]}}"#
        );
        fs::write(&path, body).unwrap();
        match read_review_class(&path) {
            Err(AttestError::InvalidReviewClass(_)) => {}
            other => panic!("expected InvalidReviewClass, got {other:?}"),
        }
    }

    #[test]
    fn review_class_read_wrong_schema_version_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("vmismatch.json");
        let body = format!(
            r#"{{"attested_sha":"{VALID_SHA}","attested_at":"2026-05-16T12:34:56Z","version":99,"classes":[{{"class":"x","sites":[{{"path":"a.rs","line":1}}]}}]}}"#
        );
        fs::write(&path, body).unwrap();
        match read_review_class(&path) {
            Err(AttestError::SchemaVersion { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, REVIEW_CLASS_SCHEMA_VERSION);
            }
            other => panic!("expected SchemaVersion error, got {other:?}"),
        }
    }

    #[test]
    fn review_class_write_leaves_no_temp_file_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("review_class_attest.json");
        let classes = vec![entry("c", &[("src/a.rs", 1)])];
        write_review_class_atomic(&path, VALID_SHA.to_string(), classes).unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            entries.iter().all(|n| !n.contains(".tmp")),
            "temp debris: {entries:?}"
        );
    }

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
