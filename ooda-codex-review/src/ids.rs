//! Branded identifier types. Each wraps a primitive with a validating
//! constructor so invalid IDs cannot be constructed outside this module.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdError {
    kind: &'static str,
    reason: String,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.kind, self.reason)
    }
}

impl std::error::Error for IdError {}

fn err(kind: &'static str, reason: impl Into<String>) -> IdError {
    IdError {
        kind,
        reason: reason.into(),
    }
}

// -- GitCommitSha ----------------------------------------------------

/// A 40-character lowercase hex git SHA.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitCommitSha(String);

impl GitCommitSha {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.len() != 40 {
            return Err(err("git sha", format!("length {} != 40", s.len())));
        }
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(err("git sha", "non-hex character"));
        }
        Ok(Self(s.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GitCommitSha {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<GitCommitSha> for String {
    fn from(v: GitCommitSha) -> Self {
        v.0
    }
}

impl fmt::Display for GitCommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- BlockerKey ------------------------------------------------------

/// Stable iteration key for stall detection. Two consecutive
/// iterations with the same `(ActionKind discriminant, BlockerKey)`
/// halt the loop with `Stalled`. The key MUST NOT embed varying
/// counts or progress markers.
///
/// No serde — `BlockerKey` is constructed and consumed entirely
/// inside the decide/runner layers; nothing serializes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct BlockerKey(String);

impl BlockerKey {
    pub fn parse(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(err("blocker key", "empty"));
        }
        Ok(Self(s))
    }

    pub(crate) fn tag(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(!s.is_empty(), "BlockerKey::tag called with empty string");
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlockerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- BranchName ------------------------------------------------------

/// A git branch name. Validated against git's `check_ref_format`
/// rules: non-empty, no `..`, no leading/trailing `/`, no leading
/// `-`, no whitespace, no control bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BranchName(String);

impl BranchName {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(err("branch name", "empty"));
        }
        if s.starts_with('-') {
            return Err(err("branch name", "leading '-'"));
        }
        if s.starts_with('/') || s.ends_with('/') {
            return Err(err("branch name", "leading or trailing '/'"));
        }
        if s.contains("..") {
            return Err(err("branch name", "contains '..'"));
        }
        if s.bytes().any(|b| b.is_ascii_whitespace() || b < 0x20) {
            return Err(err("branch name", "whitespace or control byte"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BranchName {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<BranchName> for String {
    fn from(v: BranchName) -> Self {
        v.0
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- Timestamp -------------------------------------------------------

/// An RFC-3339 / ISO-8601 timestamp parsed into a structured
/// `chrono::DateTime<Utc>`. Ord/Eq/Hash operate on the instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(chrono::DateTime<chrono::Utc>);

impl Timestamp {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(err("timestamp", "empty"));
        }
        let at = chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|e| err("timestamp", format!("parse rfc3339: {e}")))?
            .with_timezone(&chrono::Utc);
        Ok(Self(at))
    }

    pub fn at(self) -> chrono::DateTime<chrono::Utc> {
        self.0
    }
}

impl TryFrom<String> for Timestamp {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Timestamp> for String {
    fn from(v: Timestamp) -> Self {
        v.0.to_rfc3339()
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.to_rfc3339())
    }
}

// -- RepoId ----------------------------------------------------------

/// Stable identifier for a repository. Derived from
/// `git config remote.origin.url` (sha256 prefix, 12 hex chars)
/// plus the working-tree basename for human readability:
/// `<basename>-<urlhash12>`. Two worktrees of the same remote
/// share the same `RepoId` even if their on-disk paths differ.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoId(String);

impl RepoId {
    pub fn parse(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(err("repo id", "empty"));
        }
        if s.bytes().any(|b| b == b'/' || b.is_ascii_whitespace()) {
            return Err(err("repo id", "must not contain '/' or whitespace"));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RepoId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(s)
    }
}

impl From<RepoId> for String {
    fn from(v: RepoId) -> Self {
        v.0
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- ReviewMode + ReviewTarget --------------------------------------

/// Which slice of changes `codex review` is invoked against.
/// Mutually exclusive with respect to one CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewMode {
    /// `codex review --uncommitted` — working-tree changes vs HEAD.
    Uncommitted,
    /// `codex review --base <branch>` — current branch vs base.
    Base,
    /// `codex review --commit <sha>` — a specific commit.
    Commit,
    /// `codex review --pr <num>` — a specific PR's changes.
    Pr,
}

impl ReviewMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Uncommitted => "uncommitted",
            Self::Base => "base",
            Self::Commit => "commit",
            Self::Pr => "pr",
        }
    }
}

impl fmt::Display for ReviewMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The target value paired with a `ReviewMode`. `Uncommitted`
/// carries no target; the others carry a domain-typed value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum ReviewTarget {
    Uncommitted,
    Base(BranchName),
    Commit(GitCommitSha),
    Pr(u64),
}

impl ReviewTarget {
    pub fn mode(&self) -> ReviewMode {
        match self {
            Self::Uncommitted => ReviewMode::Uncommitted,
            Self::Base(_) => ReviewMode::Base,
            Self::Commit(_) => ReviewMode::Commit,
            Self::Pr(_) => ReviewMode::Pr,
        }
    }

    /// Path-safe key for recorder layout. `<mode>/<value>` or just
    /// `<mode>` for Uncommitted.
    pub fn path_key(&self) -> String {
        match self {
            Self::Uncommitted => "uncommitted".to_string(),
            Self::Base(b) => format!("base/{b}"),
            Self::Commit(s) => format!("commit/{s}"),
            Self::Pr(n) => format!("pr/{n}"),
        }
    }
}

impl fmt::Display for ReviewTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uncommitted => f.write_str("uncommitted"),
            Self::Base(b) => write!(f, "base:{b}"),
            Self::Commit(s) => write!(f, "commit:{s}"),
            Self::Pr(n) => write!(f, "pr:{n}"),
        }
    }
}

// -- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_sha_requires_40_hex_lowercase() {
        assert!(GitCommitSha::parse("").is_err());
        assert!(GitCommitSha::parse("abc").is_err());
        assert!(GitCommitSha::parse(&"g".repeat(40)).is_err());
        let upper = "A".repeat(40);
        let lower = "a".repeat(40);
        assert_eq!(GitCommitSha::parse(&upper).unwrap().as_str(), &lower);
    }

    #[test]
    fn branch_name_validates_git_ref_rules() {
        assert!(BranchName::parse("").is_err());
        assert!(BranchName::parse("-leading").is_err());
        assert!(BranchName::parse("/leading").is_err());
        assert!(BranchName::parse("trailing/").is_err());
        assert!(BranchName::parse("with..dots").is_err());
        assert!(BranchName::parse("with space").is_err());
        assert_eq!(BranchName::parse("master").unwrap().as_str(), "master");
        assert_eq!(
            BranchName::parse("feature/widget").unwrap().as_str(),
            "feature/widget"
        );
    }

    #[test]
    fn timestamp_rejects_invalid() {
        assert!(Timestamp::parse("").is_err());
        assert!(Timestamp::parse("not a timestamp").is_err());
        let t = Timestamp::parse("2026-04-23T10:00:00Z").unwrap();
        assert_eq!(t.to_string(), "2026-04-23T10:00:00+00:00");
    }

    #[test]
    fn repo_id_rejects_slash_and_ws() {
        assert!(RepoId::parse("").is_err());
        assert!(RepoId::parse("a/b").is_err());
        assert!(RepoId::parse("a b").is_err());
        assert_eq!(
            RepoId::parse("ooda-codex-review-abc123").unwrap().as_str(),
            "ooda-codex-review-abc123"
        );
    }

    #[test]
    fn review_target_path_key_shapes() {
        assert_eq!(ReviewTarget::Uncommitted.path_key(), "uncommitted");
        let b = BranchName::parse("master").unwrap();
        assert_eq!(ReviewTarget::Base(b).path_key(), "base/master");
        let s = GitCommitSha::parse(&"a".repeat(40)).unwrap();
        assert_eq!(
            ReviewTarget::Commit(s).path_key(),
            format!("commit/{}", "a".repeat(40))
        );
        assert_eq!(ReviewTarget::Pr(42).path_key(), "pr/42");
    }

    #[test]
    fn blocker_key_tag_panics_on_empty() {
        let result = std::panic::catch_unwind(|| BlockerKey::tag(""));
        assert!(result.is_err());
    }
}
