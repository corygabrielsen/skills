//! Post a rendered comment to a PR via `gh`, deduping by content
//! hash so identical state doesn't re-spam.
//!
//! The dedup state lives under the recorder-owned PR state root so
//! repeated invocations across different checkouts share the same
//! host-local memory for a given repo+PR.

use std::fs;
use std::io;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::gh::{GhError, gh_run};
use crate::recorder::Recorder;

use super::render::Rendered;

#[derive(Debug)]
pub enum PostError {
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

/// Post the comment iff its dedup key differs from the last post.
/// Returns `Ok(true)` when a comment was actually posted, `Ok(false)`
/// when suppressed by dedup.
pub fn post_if_changed(
    slug: &RepoSlug,
    pr: PullRequestNumber,
    rendered: &Rendered,
    recorder: &Recorder,
    iteration: Option<u32>,
) -> Result<bool, PostError> {
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

    let pr_s = pr.to_string();
    let slug_s = slug.to_string();
    if let Err(e) = gh_run(&[
        "pr",
        "comment",
        &pr_s,
        "-R",
        &slug_s,
        "--body",
        &rendered.body,
    ]) {
        let result = PostResult {
            prior_hash: prior,
            new_hash: key,
            posted: false,
            error: Some(e.to_string()),
        };
        recorder.record_status_comment_result(iteration, &result, "comment post failed");
        return Err(PostError::Gh(e));
    }

    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent).map_err(PostError::Hash)?;
    }
    let dedup = DedupState {
        hash: key.clone(),
        dedup_key: rendered.dedup_key.clone(),
        updated_at: Utc::now().to_rfc3339(),
    };
    let dedup_json = serde_json::to_vec_pretty(&dedup)
        .map_err(io::Error::other)
        .map_err(PostError::Hash)?;
    fs::write(&key_path, dedup_json).map_err(PostError::Hash)?;
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

/// 16-hex-char FNV-1a 64. Stable across Rust toolchain versions
/// (unlike `std::hash::DefaultHasher`), so a binary upgrade
/// doesn't silently invalidate every saved dedup hash file.
/// Not crypto — collisions just produce a redundant re-post.
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
}
