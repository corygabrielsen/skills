//! Post a rendered comment to a PR via `gh`, deduping by content
//! hash so identical state doesn't re-spam.
//!
//! Hash is stored in /tmp/ooda-pr-<owner>-<repo>-<pr>.hash. If the
//! hash matches the prior post, the new comment is suppressed.

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::gh::{gh_run, GhError};

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
) -> Result<bool, PostError> {
    let key_path = hash_path(slug, pr);
    let prior = fs::read_to_string(&key_path).ok();
    let key = hash_str(&rendered.dedup_key);

    if prior.as_deref().map(str::trim) == Some(key.as_str()) {
        return Ok(false);
    }

    let pr_s = pr.to_string();
    let slug_s = slug.to_string();
    gh_run(&[
        "pr",
        "comment",
        &pr_s,
        "-R",
        &slug_s,
        "--body",
        &rendered.body,
    ])?;

    fs::write(&key_path, format!("{key}\n")).map_err(PostError::Hash)?;
    Ok(true)
}

fn hash_path(slug: &RepoSlug, pr: PullRequestNumber) -> PathBuf {
    let owner = slug.owner();
    let repo = slug.repo();
    PathBuf::from(format!("/tmp/ooda-pr-{owner}-{repo}-{pr}.hash"))
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
}
