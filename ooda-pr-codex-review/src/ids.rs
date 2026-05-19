//! Branded identifier types.
//!
//! Each wraps a primitive (`String` / `u64` / instant) behind a
//! validating constructor. Two invariants hold:
//!
//! - **Parse, don't validate**: a value of the branded type is
//!   evidence that its domain rule has already passed. Downstream
//!   code never re-checks.
//! - **Cross-domain distinctness**: types that share a primitive
//!   carrier (branch name, team name, check name) are
//!   structurally distinct, so a "right shape, wrong namespace"
//!   bug is a compile error.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdError {
    kind: &'static str,
    reason: String,
}

impl IdError {
    /// Public constructor for sibling modules that define their own
    /// branded newtypes (e.g. [`crate::orient::thread::ThreadId`])
    /// and reuse the shared error shape.
    pub fn new(kind: &'static str, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.kind, self.reason)
    }
}

impl std::error::Error for IdError {}

fn err(kind: &'static str, reason: impl Into<String>) -> IdError {
    IdError::new(kind, reason)
}

/// Validate a GitHub account login (user or org). GitHub's own rule
/// is `[A-Za-z0-9-]{1,39}` with no leading or trailing `-` and no
/// consecutive `-`. Applied to [`Owner`].
fn validate_github_account_login(kind: &'static str, s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(err(kind, "empty"));
    }
    if s.len() > 39 {
        return Err(err(kind, "length > 39"));
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err(err(kind, "leading or trailing '-'"));
    }
    if s.contains("--") {
        return Err(err(kind, "consecutive '-'"));
    }
    if !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(err(kind, "non-[A-Za-z0-9-] byte"));
    }
    Ok(())
}

/// Validate a GitHub repo name: `[A-Za-z0-9._-]{1,100}`, no leading
/// `.` or `-`. Applied to [`Repo`].
fn validate_github_repo_name(kind: &'static str, s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(err(kind, "empty"));
    }
    if s.len() > 100 {
        return Err(err(kind, "length > 100"));
    }
    if s.starts_with('.') || s.starts_with('-') {
        return Err(err(kind, "leading '.' or '-'"));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(err(kind, "non-[A-Za-z0-9._-] byte"));
    }
    Ok(())
}

/// Reject empty / whitespace / control-byte strings. Used by
/// newtypes whose surface domain (team slug, check name, login
/// including `[bot]` suffix) tolerates richer character sets but
/// MUST NOT carry framing-significant bytes.
fn validate_no_control_bytes(kind: &'static str, s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(err(kind, "empty"));
    }
    if s.trim().is_empty() {
        return Err(err(kind, "whitespace-only"));
    }
    if s.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(err(kind, "contains ASCII control byte"));
    }
    Ok(())
}

// -- Owner -----------------------------------------------------------

/// A GitHub account login (user or org) used as a repo owner.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Owner(String);

impl Owner {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate_github_account_login("owner", s)?;
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
        validate_github_repo_name("repo", s)?;
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

/// A GitHub account login. Three observed surface forms, all valid:
///
/// - `<name>` — plain user or org login
/// - `<name>[bot]` — bot login from REST / GraphQL Bot-typed actor
/// - `app/<slug>` — GitHub App identity as returned by `gh pr view`'s
///   `author` field for App-authored PRs (e.g. dependabot opens PRs
///   in this form)
///
/// Bot identity is encoded by either the `[bot]` suffix or the
/// `app/` prefix, so [`is_bot`] inspects both rather than carrying
/// a separate flag.
///
/// [`is_bot`]: GitHubLogin::is_bot
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitHubLogin(String);

impl GitHubLogin {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        // Three forms reduce to a stem that's a regular account
        // login (charset `[A-Za-z0-9-]{1,39}`). Order matters: check
        // `app/` first because the slug itself could in principle
        // end in `[bot]`.
        let stem = if let Some(rest) = s.strip_prefix("app/") {
            rest
        } else if let Some(rest) = s.strip_suffix("[bot]") {
            rest
        } else {
            s
        };
        validate_github_account_login("github login", stem)?;
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_bot(&self) -> bool {
        self.0.ends_with("[bot]") || self.0.starts_with("app/")
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

/// Stable iteration key for stall detection.
///
/// Invariant: a `BlockerKey` identifies a *gate*, not the current
/// witnesses behind it. Two consecutive iterations with the same
/// `(action discriminant, BlockerKey)` halt the loop as stalled, so
/// any progress marker (count, cohort identity, timestamp) embedded
/// in the key would mask real progress as repetition. Per-iteration
/// payload travels on the action; the key names what is gated.
///
/// Re-exported from [`ooda_core::BlockerKey`]; the type and its
/// validating constructor live in the shared crate.
pub(crate) use ooda_core::BlockerKey;

// -- BranchName ------------------------------------------------------

/// A git branch name. Constructor enforces git's `check_ref_format`
/// subset: non-empty, no `..`, no leading or trailing `/`, no leading
/// `-`, no whitespace, no control bytes.
///
/// The newtype keeps branch identifiers structurally distinct from
/// other primitives carried as `String`, so passing one in the
/// position of another is a type error rather than a silent confusion.
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

/// A GitHub team identifier. Lives in a different namespace from
/// [`GitHubLogin`]: both are non-empty strings, but they index
/// distinct GitHub primitives (account vs. group of accounts).
///
/// The newtype makes that distinction structural, so a team handle
/// cannot be passed where a login is expected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TeamName(String);

impl TeamName {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate_no_control_bytes("team name", s)?;
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

/// A pending PR reviewer — either a user account or a team.
/// The sum is symmetric over [`GitHubLogin`] and [`TeamName`]; each
/// arm carries the appropriate validated identifier.
///
/// `Display` writes the underlying identifier verbatim: the rendered
/// form matches what the reviewer surface in GitHub's UI shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
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

/// A GitHub status-check / check-run name.
///
/// The wire form admits spaces, slashes, and unicode; the only
/// constructor invariant is non-emptiness. Anything stricter would
/// reject names the GitHub API already accepts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CheckName(String);

impl CheckName {
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate_no_control_bytes("check name", s)?;
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

// A check name is gate-stable: identity is preserved across
// iterations and distinct gates carry distinct names.
impl ooda_core::GateIdentity for CheckName {}

// -- Timestamp -------------------------------------------------------

/// An instant in UTC, parsed from RFC-3339 / ISO-8601.
///
/// Equality and ordering are over the underlying instant, not its
/// surface form: two timestamps written with different offsets that
/// denote the same instant compare equal. `Display` normalises to a
/// canonical RFC-3339 rendering.
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

// -- CodexReasoningLevel --------------------------------------------------

/// Reasoning effort level for the codex-review axis. The variants
/// form a four-rung totally ordered ladder; [`higher`] and [`lower`]
/// project that ladder to the canonical climb / drop morphisms,
/// returning `None` at the endpoints.
///
/// The lowercase token (`low`, `medium`, `high`, `xhigh`) is the
/// stable surface form across every external boundary the axis
/// touches.
///
/// [`higher`]: CodexReasoningLevel::higher
/// [`lower`]: CodexReasoningLevel::lower
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexReasoningLevel {
    Low,
    Medium,
    High,
    Xhigh,
}

impl CodexReasoningLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    pub fn higher(self) -> Option<Self> {
        match self {
            Self::Low => Some(Self::Medium),
            Self::Medium => Some(Self::High),
            Self::High => Some(Self::Xhigh),
            Self::Xhigh => None,
        }
    }

    pub fn lower(self) -> Option<Self> {
        match self {
            Self::Xhigh => Some(Self::High),
            Self::High => Some(Self::Medium),
            Self::Medium => Some(Self::Low),
            Self::Low => None,
        }
    }
}

impl fmt::Display for CodexReasoningLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// Gate-stable: every level projects to a single fixed token, and
// distinct levels project to distinct tokens.
impl ooda_core::GateIdentity for CodexReasoningLevel {}

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
    fn owner_rejects_whitespace_control_and_disallowed_chars() {
        assert!(Owner::parse("al ice").is_err());
        assert!(Owner::parse("acme\n").is_err());
        assert!(Owner::parse("acme[bot]").is_err());
        assert!(Owner::parse("-leading").is_err());
        assert!(Owner::parse("trailing-").is_err());
        assert!(Owner::parse("foo--bar").is_err());
        // 40 chars exceeds the 39-byte ceiling.
        assert!(Owner::parse(&"a".repeat(40)).is_err());
        // 39 chars is exactly the ceiling.
        assert_eq!(
            Owner::parse(&"a".repeat(39)).unwrap().as_str(),
            "a".repeat(39),
        );
    }

    #[test]
    fn repo_rejects_empty_and_slash() {
        assert!(Repo::parse("").is_err());
        assert!(Repo::parse("a/b").is_err());
        assert_eq!(Repo::parse("protocol").unwrap().as_str(), "protocol");
    }

    #[test]
    fn repo_rejects_whitespace_control_and_disallowed_chars() {
        assert!(Repo::parse("acme widget").is_err());
        assert!(Repo::parse("acme\n").is_err());
        assert!(Repo::parse(".hidden").is_err());
        assert!(Repo::parse("-leading").is_err());
        // 101 chars exceeds the 100-byte ceiling.
        assert!(Repo::parse(&"a".repeat(101)).is_err());
        // Dots, underscores, and dashes are admitted in non-leading position.
        assert_eq!(
            Repo::parse("protocol.v2-stable_1").unwrap().as_str(),
            "protocol.v2-stable_1",
        );
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
    fn pull_request_number_rejects_zero() {
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
        // Surface form is normalised; case does not survive parsing.
        assert_eq!(GitCommitSha::parse(&upper).unwrap().as_str(), &lower);
    }

    #[test]
    fn github_login_detects_bot_suffix() {
        assert!(GitHubLogin::parse("").is_err());
        assert!(!GitHubLogin::parse("alice").unwrap().is_bot());
        assert!(GitHubLogin::parse("copilot[bot]").unwrap().is_bot());
    }

    /// Regression: `gh pr view --json author` returns
    /// `{"login":"app/dependabot"}` for App-authored PRs (observed
    /// against w3-io/w3-explorer#353). The parser used to reject
    /// the `/` byte and the binary crashed with `BinaryError` 70.
    #[test]
    fn github_login_accepts_app_slug_form() {
        let l = GitHubLogin::parse("app/dependabot").expect("valid app/ form");
        assert!(l.is_bot(), "app/* logins are bots");
        assert_eq!(l.as_str(), "app/dependabot", "surface form preserved");
        // Other observed app slugs.
        assert!(GitHubLogin::parse("app/copilot-pull-request-reviewer").is_ok());
        // The slug part is still validated against the account
        // login charset.
        assert!(GitHubLogin::parse("app/has space").is_err());
        assert!(GitHubLogin::parse("app/").is_err());
        assert!(GitHubLogin::parse("app/-leading").is_err());
    }

    #[test]
    fn github_login_rejects_control_bytes_and_invalid_chars() {
        assert!(GitHubLogin::parse("al ice").is_err());
        assert!(GitHubLogin::parse("alice\n").is_err());
        assert!(GitHubLogin::parse("-alice").is_err());
        assert!(GitHubLogin::parse("alice--bob").is_err());
        // The stem before `[bot]` is the part the GitHub regex applies to.
        assert!(GitHubLogin::parse("[bot]").is_err());
        assert!(GitHubLogin::parse("al ice[bot]").is_err());
        assert!(GitHubLogin::parse("copilot-pull-request-reviewer[bot]").is_ok());
    }

    #[test]
    fn team_name_rejects_control_bytes() {
        assert!(TeamName::parse("").is_err());
        assert!(TeamName::parse("   ").is_err());
        assert!(TeamName::parse("backend\n").is_err());
        assert_eq!(TeamName::parse("backend").unwrap().as_str(), "backend",);
    }

    #[test]
    fn check_name_rejects_control_bytes() {
        // Spaces and slashes are admitted — only control bytes are
        // rejected so stderr framing cannot be split.
        assert!(CheckName::parse("CI\nstage").is_err());
        assert!(CheckName::parse("CI\0stage").is_err());
        assert_eq!(
            CheckName::parse("Build / test").unwrap().as_str(),
            "Build / test",
        );
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
        // Display is the canonical surface form, not the input form.
        assert_eq!(t.to_string(), "2026-04-23T10:00:00+00:00");
        assert_eq!(t.at().to_rfc3339(), "2026-04-23T10:00:00+00:00");
    }

    #[test]
    fn timestamp_orders_by_time_not_lexicographic() {
        // Equality and ordering range over the instant, not the
        // surface form: two renderings of the same instant must
        // compare equal even when their bytes differ.
        let t1 = Timestamp::parse("2026-04-23T10:00:00Z").unwrap();
        let t2 = Timestamp::parse("2026-04-23T10:00:00+00:00").unwrap();
        let t3 = Timestamp::parse("2026-04-23T11:00:00Z").unwrap();
        assert!(t1 < t3);
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

    // ── CodexReasoningLevel ladder boundary cases ──

    #[test]
    fn codex_level_higher_climbs_one_rung() {
        assert_eq!(
            CodexReasoningLevel::Low.higher(),
            Some(CodexReasoningLevel::Medium),
        );
        assert_eq!(
            CodexReasoningLevel::Medium.higher(),
            Some(CodexReasoningLevel::High),
        );
        assert_eq!(
            CodexReasoningLevel::High.higher(),
            Some(CodexReasoningLevel::Xhigh),
        );
    }

    #[test]
    fn codex_level_higher_at_ceiling_yields_none() {
        assert_eq!(CodexReasoningLevel::Xhigh.higher(), None);
    }

    #[test]
    fn codex_level_lower_drops_one_rung() {
        assert_eq!(
            CodexReasoningLevel::Xhigh.lower(),
            Some(CodexReasoningLevel::High),
        );
        assert_eq!(
            CodexReasoningLevel::High.lower(),
            Some(CodexReasoningLevel::Medium),
        );
        assert_eq!(
            CodexReasoningLevel::Medium.lower(),
            Some(CodexReasoningLevel::Low),
        );
    }

    #[test]
    fn codex_level_lower_at_floor_yields_none() {
        assert_eq!(CodexReasoningLevel::Low.lower(), None);
    }
}
