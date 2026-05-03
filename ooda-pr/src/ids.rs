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

// -- BlockerKey ------------------------------------------------------

/// Stable iteration key for stall detection. Two consecutive
/// iterations with the same `(ActionKind discriminant, BlockerKey)`
/// halt the loop with `Stalled`. The key MUST NOT embed varying
/// counts or progress markers — the comment in `decide/reviews.rs`
/// (`AddressThreads { count }`) calls this out explicitly: "count
/// lives in ActionKind, never in the blocker string."
///
/// Newtype promotes that invariant from a comment to a type
/// distinction: a `String` of human-readable text can no longer be
/// passed where a stall key is expected.
///
/// No serde — `BlockerKey` is constructed and consumed entirely
/// inside the decide/runner layers; nothing serializes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlockerKey(String);

impl BlockerKey {
    /// Validating constructor for arbitrary input. Use for any
    /// value not known at the call site to be non-empty.
    pub fn parse(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(err("blocker key", "empty"));
        }
        Ok(Self(s))
    }

    /// Infallible constructor for internal, known-non-empty
    /// construction (literal prefixes + typed payloads in the
    /// decide layer). Panics if the input is empty — that would
    /// be a programmer error in the caller, not user input. The
    /// `Self` return signals "construction is intended to succeed"
    /// to the reader, where `parse(...).expect(...)` would suggest
    /// a fallible operation.
    ///
    /// `pub(crate)` to constrain the panic surface — external
    /// callers must go through `parse` and handle the `Result`.
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
/// `-`, no whitespace, no control bytes. Permits the punctuation
/// branch names actually use (`feature/foo`, `release-1.2`, etc.).
///
/// Newtype prevents the cross-domain confusion that's possible
/// when branch names, ruleset names, team names, and CI check
/// names all flow through `String`.
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

// -- TeamName --------------------------------------------------------

/// A GitHub team identifier as returned by GraphQL
/// `RequestedReviewer { ... on Team { name } }`. Lives in a
/// different namespace from `GitHubLogin` (a user/bot login):
/// `Cory` is a login, `backend-team` is a team. Both are
/// non-empty strings, but they index distinct GitHub primitives.
///
/// Newtype prevents the "looks the same, means different things"
/// confusion that `Vec<String>` had previously.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TeamName(String);

impl TeamName {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(err("team name", "empty"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TeamName {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<TeamName> for String {
    fn from(v: TeamName) -> Self {
        v.0
    }
}

impl fmt::Display for TeamName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- Reviewer --------------------------------------------------------

/// A pending PR reviewer. GitHub allows two distinct kinds: a
/// concrete user identity ([`GitHubLogin`]) or a team ([`TeamName`]).
/// Both arms now carry validated newtypes — the sum is symmetric
/// and structurally distinguishes the two GitHub primitives.
///
/// `Display` writes the login or team name verbatim — both forms
/// are what GitHub's UI shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Reviewer {
    User(GitHubLogin),
    Team(TeamName),
}

impl fmt::Display for Reviewer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(login) => write!(f, "{login}"),
            Self::Team(name) => write!(f, "{name}"),
        }
    }
}

// -- CheckName -------------------------------------------------------

/// A GitHub status-check / check-run name (e.g. `Build / test`).
/// Names may contain spaces, slashes, and unicode — the only
/// invariant is non-emptiness.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CheckName(String);

impl CheckName {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if s.is_empty() {
            return Err(err("check name", "empty"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CheckName {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<CheckName> for String {
    fn from(v: CheckName) -> Self {
        v.0
    }
}

impl fmt::Display for CheckName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// -- Timestamp -------------------------------------------------------

/// An RFC-3339 / ISO-8601 timestamp parsed into a structured
/// `chrono::DateTime<Utc>`. `Ord`/`Eq`/`Hash` operate on the
/// instant — surface forms (`...Z` vs `...+00:00`) representing the
/// same instant compare equal. Display normalizes to RFC-3339 with
/// `+00:00` suffix; nothing downstream depends on byte-identity.
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
            "feature/widget",
        );
        assert_eq!(
            BranchName::parse("release-1.2").unwrap().as_str(),
            "release-1.2",
        );
    }

    #[test]
    fn check_name_rejects_empty() {
        assert!(CheckName::parse("").is_err());
        let n = CheckName::parse("Build / test").unwrap();
        assert_eq!(n.as_str(), "Build / test");
    }

    #[test]
    fn timestamp_rejects_empty_and_invalid() {
        assert!(Timestamp::parse("").is_err());
        assert!(Timestamp::parse("not a timestamp").is_err());
        let t = Timestamp::parse("2026-04-23T10:00:00Z").unwrap();
        // Display normalizes the surface form to RFC-3339 with explicit
        // offset; nothing downstream depends on byte-identity.
        assert_eq!(t.to_string(), "2026-04-23T10:00:00+00:00");
        assert_eq!(t.at().to_rfc3339(), "2026-04-23T10:00:00+00:00");
    }

    #[test]
    fn timestamp_orders_by_time_not_lexicographic() {
        // Lexicographic ordering coincides with chronological for
        // ISO-8601 UTC, but breaks for offset variants. Exercise
        // both to lock the structural-comparison invariant.
        let t1 = Timestamp::parse("2026-04-23T10:00:00Z").unwrap();
        let t2 = Timestamp::parse("2026-04-23T10:00:00+00:00").unwrap();
        let t3 = Timestamp::parse("2026-04-23T11:00:00Z").unwrap();
        assert!(t1 < t3);
        // Same instant in different surface forms: equal under Ord
        // AND under Eq (byte-identity is not preserved).
        assert_eq!(t1.cmp(&t2), std::cmp::Ordering::Equal);
        assert_eq!(t1, t2);
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
