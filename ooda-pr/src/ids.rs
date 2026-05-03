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

// -- Owner -----------------------------------------------------------

/// A GitHub account login (user or org) used as a repo owner.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Owner(String);

impl Owner {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(err("owner", "empty"));
        }
        if s.contains('/') {
            return Err(err("owner", "contains '/'"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Owner {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Owner> for String {
    fn from(v: Owner) -> Self {
        v.0
    }
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- Repo ------------------------------------------------------------

/// A repository name (without the owner prefix).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Repo(String);

impl Repo {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(err("repo", "empty"));
        }
        if s.contains('/') {
            return Err(err("repo", "contains '/'"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Repo {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Repo> for String {
    fn from(v: Repo) -> Self {
        v.0
    }
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- RepoSlug --------------------------------------------------------

/// `owner/repo` pair, parsed and split.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoSlug {
    owner: Owner,
    repo: Repo,
}

impl RepoSlug {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let (o, r) = s
            .split_once('/')
            .ok_or_else(|| err("repo slug", "missing '/'"))?;
        if r.contains('/') {
            return Err(err("repo slug", "more than one '/'"));
        }
        Ok(Self {
            owner: Owner::parse(o)?,
            repo: Repo::parse(r)?,
        })
    }

    pub fn new(owner: Owner, repo: Repo) -> Self {
        Self { owner, repo }
    }

    pub fn owner(&self) -> &Owner {
        &self.owner
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }
}

impl TryFrom<String> for RepoSlug {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<RepoSlug> for String {
    fn from(v: RepoSlug) -> Self {
        format!("{}/{}", v.owner, v.repo)
    }
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

// -- PullRequestNumber ----------------------------------------------

/// A PR number; always positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct PullRequestNumber(u64);

impl PullRequestNumber {
    pub fn new(n: u64) -> Result<Self, IdError> {
        if n == 0 {
            return Err(err("pull request number", "must be > 0"));
        }
        Ok(Self(n))
    }

    pub fn parse(s: &str) -> Result<Self, IdError> {
        let n: u64 = s
            .parse()
            .map_err(|_| err("pull request number", format!("not a number: {s}")))?;
        Self::new(n)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for PullRequestNumber {
    type Error = IdError;
    fn try_from(n: u64) -> Result<Self, Self::Error> {
        Self::new(n)
    }
}

impl From<PullRequestNumber> for u64 {
    fn from(v: PullRequestNumber) -> Self {
        v.0
    }
}

impl fmt::Display for PullRequestNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// -- GitCommitSha ----------------------------------------------------

/// A 40-character lowercase hex git SHA (GitHub's canonical form).
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
        let normalized = s.to_ascii_lowercase();
        Ok(Self(normalized))
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

// -- GitHubLogin -----------------------------------------------------

/// A GitHub account login. Bot accounts canonically end in `[bot]`
/// when returned by REST; GraphQL may return the unsuffixed form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitHubLogin(String);

impl GitHubLogin {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(err("github login", "empty"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_bot(&self) -> bool {
        self.0.ends_with("[bot]")
    }
}

impl TryFrom<String> for GitHubLogin {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<GitHubLogin> for String {
    fn from(v: GitHubLogin) -> Self {
        v.0
    }
}

impl fmt::Display for GitHubLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- Timestamp -------------------------------------------------------

/// An ISO-8601 timestamp, kept as a string at this layer. Parsing to
/// a structured time value is orient's job (or deferred until needed).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(String);

impl Timestamp {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(err("timestamp", "empty"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
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
        v.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_rejects_empty_and_slash() {
        assert!(Owner::parse("").is_err());
        assert!(Owner::parse("a/b").is_err());
        assert_eq!(Owner::parse("acme").unwrap().as_str(), "acme");
    }

    #[test]
    fn repo_rejects_empty_and_slash() {
        assert!(Repo::parse("").is_err());
        assert!(Repo::parse("a/b").is_err());
        assert_eq!(Repo::parse("protocol").unwrap().as_str(), "protocol");
    }

    #[test]
    fn repo_slug_requires_exactly_one_slash() {
        assert!(RepoSlug::parse("noslash").is_err());
        assert!(RepoSlug::parse("a/b/c").is_err());
        let s = RepoSlug::parse("acme/protocol").unwrap();
        assert_eq!(s.owner().as_str(), "acme");
        assert_eq!(s.repo().as_str(), "protocol");
        assert_eq!(s.to_string(), "acme/protocol");
    }

    #[test]
    fn pr_number_rejects_zero() {
        assert!(PullRequestNumber::new(0).is_err());
        assert_eq!(PullRequestNumber::new(614).unwrap().get(), 614);
        assert_eq!(PullRequestNumber::parse("614").unwrap().get(), 614);
        assert!(PullRequestNumber::parse("abc").is_err());
    }

    #[test]
    fn git_sha_requires_40_hex_lowercase() {
        assert!(GitCommitSha::parse("").is_err());
        assert!(GitCommitSha::parse("abc").is_err());
        assert!(GitCommitSha::parse(&"g".repeat(40)).is_err());
        let upper = "A".repeat(40);
        let lower = "a".repeat(40);
        // Uppercase input normalizes to lowercase.
        assert_eq!(GitCommitSha::parse(&upper).unwrap().as_str(), &lower);
    }

    #[test]
    fn github_login_detects_bot_suffix() {
        assert!(GitHubLogin::parse("").is_err());
        assert!(!GitHubLogin::parse("alice").unwrap().is_bot());
        assert!(GitHubLogin::parse("copilot[bot]").unwrap().is_bot());
    }

    #[test]
    fn timestamp_rejects_empty() {
        assert!(Timestamp::parse("").is_err());
        assert_eq!(
            Timestamp::parse("2026-04-23T10:00:00Z").unwrap().as_str(),
            "2026-04-23T10:00:00Z",
        );
    }

    #[test]
    fn serde_roundtrip_repo_slug() {
        let s = RepoSlug::parse("acme/protocol").unwrap();
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(j, "\"acme/protocol\"");
        let back: RepoSlug = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn serde_rejects_invalid_on_deserialize() {
        let r: Result<RepoSlug, _> = serde_json::from_str("\"noslash\"");
        assert!(r.is_err());
    }
}
