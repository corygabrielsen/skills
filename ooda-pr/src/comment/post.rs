//! Deliver a rendered comment, deduping by content hash so
//! identical structural state does not re-post.
//!
//! Dedup memory is per-PR and host-local, kept under the
//! recorder's state tree so repeated invocations from different
//! working copies share the same suppression record.

use std::fs;
use std::io;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::gh::{GhError, gh_run};
use crate::recorder::Recorder;

use super::render::Rendered;

#[derive(Debug)]
pub(crate) enum PostError {
    Gh(GhError),
    Hash(io::Error),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gh(e) => write!(f, "{e}"),
            Self::Hash(e) => write!(f, "hash file: {e}"),
        }
    }
}

impl std::error::Error for PostError {}

impl From<GhError> for PostError {
    fn from(e: GhError) -> Self {
        Self::Gh(e)
    }
}

/// Deliver the comment unless its dedup key matches the prior
/// post's. `Ok(true)` ⇒ delivered; `Ok(false)` ⇒ suppressed.
pub(crate) fn post_if_changed(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    rendered: &Rendered,
    recorder: &Recorder,
    iteration: Option<u32>,
) -> Result<bool, PostError> {
    let pr_s = pr.to_string();
    let slug_s = slug.to_string();
    let body = rendered.body.clone();
    post_if_changed_with(rendered, recorder, iteration, move || {
        gh_run(&["pr", "comment", &pr_s, "-R", &slug_s, "--body", &body])
    })
}

/// Inner form parameterised on delivery. The closure performs the
/// upstream call so the three control branches (dedup-skip,
/// delivery + state-write, delivery error) are driven by the
/// closure's return without spawning anything. The public entry
/// is a thin shim over this so the caller surface stays narrow.
fn post_if_changed_with<F>(
    rendered: &Rendered,
    recorder: &Recorder,
    iteration: Option<u32>,
    post: F,
) -> Result<bool, PostError>
where
    F: FnOnce() -> Result<(), GhError>,
{
    let key_path = recorder.dedup_path();
    let prior = read_prior_hash(&key_path).map_err(PostError::Hash)?;
    let key = hash_str(&rendered.dedup_key);

    if prior.as_deref() == Some(key.as_str()) {
        let result = PostResult {
            prior_hash: prior,
            new_hash: key,
            posted: false,
            error: None,
        };
        recorder.record_status_comment_result(iteration, &result, "comment skipped (unchanged)");
        return Ok(false);
    }

    if let Err(e) = post() {
        let result = PostResult {
            prior_hash: prior,
            new_hash: key,
            posted: false,
            error: Some(e.to_string()),
        };
        recorder.record_status_comment_result(iteration, &result, "comment post failed");
        return Err(PostError::Gh(e));
    }

    let dedup = DedupState {
        hash: key.clone(),
        dedup_key: rendered.dedup_key.clone(),
        updated_at: Utc::now().to_rfc3339(),
    };
    let dedup_json = serde_json::to_vec_pretty(&dedup)
        .map_err(io::Error::other)
        .map_err(PostError::Hash)?;
    // Atomic + durable write. The dedup file is a stable read
    // surface: a torn write would corrupt the parse fallback into
    // a non-matching prior hash on the next start and the
    // suppression invariant would silently break.
    ooda_core::atomic_io::write_atomic(&key_path, &dedup_json).map_err(PostError::Hash)?;
    let result = PostResult {
        prior_hash: prior,
        new_hash: key,
        posted: true,
        error: None,
    };
    recorder.record_status_comment_result(iteration, &result, "comment posted");
    Ok(true)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DedupState {
    hash: String,
    dedup_key: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct PostResult {
    prior_hash: Option<String>,
    new_hash: String,
    posted: bool,
    error: Option<String>,
}

fn read_prior_hash(path: &std::path::Path) -> Result<Option<String>, io::Error> {
    match fs::read_to_string(path) {
        Ok(body) => Ok(parse_prior_hash(&body)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn parse_prior_hash(body: &str) -> Option<String> {
    serde_json::from_str::<DedupState>(body)
        .map(|d| d.hash)
        .ok()
        .or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
}

/// Toolchain-stable 64-bit FNV-1a, rendered as 16 hex chars.
/// Stability is the invariant: a hashing change would silently
/// invalidate every existing dedup record. Not cryptographic; a
/// collision only produces a redundant re-post.
fn hash_str(s: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x100_0000_01b3;
    let mut h: u64 = FNV_OFFSET;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::{Recorder, RecorderConfig, RunMode};
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn hash_str_is_stable() {
        assert_eq!(hash_str("hello"), hash_str("hello"));
    }

    #[test]
    fn hash_str_distinguishes_distinct_input() {
        assert_ne!(hash_str("hello"), hash_str("world"));
    }

    #[test]
    fn hash_str_is_16_hex_chars() {
        let h = hash_str("anything");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_prior_hash_accepts_json_state() {
        let hash = parse_prior_hash(r#"{"hash":"abc","dedup_key":"x","updated_at":"now"}"#);
        assert_eq!(hash.as_deref(), Some("abc"));
    }

    #[test]
    fn parse_prior_hash_accepts_legacy_plain_hash() {
        let hash = parse_prior_hash("abc\n");
        assert_eq!(hash.as_deref(), Some("abc"));
    }

    // ── post_if_changed_with branch coverage ──
    //
    // The three control branches (dedup-skip, deliver + write,
    // delivery error) are exercised via the injected closure so
    // behaviour is deterministic without spawning a subprocess.

    fn temp_root(label: &str) -> std::path::PathBuf {
        // Per-test unique directory (process id + monotonic
        // counter) prevents concurrent test runs from racing on
        // the same recorder tree.
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "ooda-pr-post-test-{label}-{}-{seq}",
            std::process::id()
        ))
    }

    fn open_recorder(root: &std::path::Path) -> Recorder {
        let _ = fs::remove_dir_all(root);
        Recorder::open(RecorderConfig {
            slug: RepoSlug::parse("acme/widgets").unwrap(),
            pr: PullRequestNumber::new(42).unwrap(),
            mode: RunMode::Loop,
            max_iter: std::num::NonZeroU32::new(1).expect("1 is non-zero"),
            status_comment: true,
            state_root: Some(root.to_path_buf()),
            legacy_trace: None,
        })
        .unwrap()
    }

    fn sample_rendered() -> Rendered {
        Rendered {
            body: "## OODA · acme/widgets#42\nstuff".to_string(),
            dedup_key: "ci:pass|reviews:none|exec".to_string(),
        }
    }

    #[test]
    fn post_if_changed_with_skips_when_dedup_key_matches() {
        let root = temp_root("skip");
        let recorder = open_recorder(&root);
        // Pre-seed the dedup file with the matching hash.
        let rendered = sample_rendered();
        let key = hash_str(&rendered.dedup_key);
        let dedup = DedupState {
            hash: key.clone(),
            dedup_key: rendered.dedup_key.clone(),
            updated_at: "prior".to_string(),
        };
        let dedup_path = recorder.dedup_path();
        if let Some(parent) = dedup_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&dedup_path, serde_json::to_vec(&dedup).unwrap()).unwrap();

        // Invocation flag: the skip branch must not call delivery.
        let invoked = std::cell::Cell::new(false);
        let posted = post_if_changed_with(&rendered, &recorder, Some(1), || {
            invoked.set(true);
            Ok(())
        })
        .unwrap();

        assert!(!posted, "dedup-skip must report Ok(false)");
        assert!(!invoked.get(), "dedup-skip must not invoke the poster");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn post_if_changed_with_posts_and_writes_dedup_on_miss() {
        let root = temp_root("post");
        let recorder = open_recorder(&root);
        let rendered = sample_rendered();
        let dedup_path = recorder.dedup_path();
        // No prior file present → dedup miss.
        assert!(!dedup_path.exists());

        let invoked = std::cell::Cell::new(false);
        let posted = post_if_changed_with(&rendered, &recorder, Some(1), || {
            invoked.set(true);
            Ok(())
        })
        .unwrap();

        assert!(posted, "successful post returns Ok(true)");
        assert!(invoked.get(), "poster must be invoked on dedup miss");
        assert!(
            dedup_path.exists(),
            "dedup state must be written after post"
        );

        let body = fs::read_to_string(&dedup_path).unwrap();
        let stored: DedupState = serde_json::from_str(&body).unwrap();
        assert_eq!(stored.hash, hash_str(&rendered.dedup_key));
        assert_eq!(stored.dedup_key, rendered.dedup_key);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn post_if_changed_with_propagates_post_error_and_skips_state_write() {
        let root = temp_root("err");
        let recorder = open_recorder(&root);
        let rendered = sample_rendered();
        let dedup_path = recorder.dedup_path();
        assert!(!dedup_path.exists());

        let err = post_if_changed_with(&rendered, &recorder, Some(1), || {
            Err(GhError::NonZero {
                code: Some(1),
                stderr: "synthetic failure".to_string(),
            })
        })
        .unwrap_err();
        match err {
            PostError::Gh(GhError::NonZero { code, .. }) => assert_eq!(code, Some(1)),
            other => panic!("expected Gh(NonZero), got {other:?}"),
        }
        assert!(
            !dedup_path.exists(),
            "dedup state must not be written when post errors",
        );
        let _ = fs::remove_dir_all(root);
    }
}
