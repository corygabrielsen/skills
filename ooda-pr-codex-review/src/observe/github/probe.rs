//! Change probe over a pull request's REST surface: one conditional
//! `GET` per endpoint per pass, reporting the first endpoint whose
//! projected state differs from its baseline.
//!
//! # Invariants
//!
//! - **Projection, not bytes**: an endpoint changes iff its
//!   projection changes. Each projection keeps the resource's own
//!   fields and drops embedded neighbours (repository objects,
//!   in-progress output, app metadata) whose churn says nothing
//!   about this pull request.
//! - **Baselines persist across iterations**: a validator is
//!   advanced only by a probe pass, never by observe, so a change
//!   landing between a pass and the next observe wakes the following
//!   wait instead of being absorbed. An endpoint seen for the first
//!   time establishes its baseline and never wakes.
//! - **Frontier pages**: append-only lists are read at the page the
//!   next item lands on, `count / per_page + 1`. Edits below the
//!   frontier are not observed; the wait's deadline bounds that miss.
//! - **Comments by server time**: issue comments are read `since` the
//!   newest update the last observe saw, and change iff an item is
//!   newer or is an unseen comment updated at that same instant. The
//!   cursor is GitHub's own timestamp, immune to local clock skew.
//! - **Whole passes**: a pass refreshes every endpoint it reaches
//!   before reporting the first change, so one event that moves
//!   several endpoints wakes once.
//! - **Budget, not failure**: a pass that runs out of the wait's time
//!   ends quietly; only an endpoint's own error is a failure.
//! - **REST only**: GraphQL has no conditional requests; review
//!   thread resolution is not probed and surfaces at the deadline.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ooda_core::SingleLineString;
use ooda_core::wait::{ChangeProbe, ProbeResult};
use serde_json::{Map, Value};

use super::gh::{Conditional, GhError, gh_conditional};
use crate::ids::{GitCommitSha, PullRequestNumber, RepoSlug, Timestamp};

/// Page size every list endpoint is read at; the frontier page index
/// depends on it.
const PER_PAGE: usize = 100;

/// Upper bound on one conditional `GET`, before the wait's own
/// remaining time.
const CALL_DEADLINE: Duration = Duration::from_secs(10);

/// Least remaining time a pass starts another request with; a request
/// cut shorter would time out on an endpoint that is merely slow.
pub(crate) const MIN_CALL_BUDGET: Duration = Duration::from_secs(2);

/// Most time [`GitHubProbe::prime`] spends establishing baselines.
pub(crate) const PRIME_BUDGET: Duration = Duration::from_secs(10);

/// What the last observe saw, which positions every endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeSeed {
    pub head: GitCommitSha,
    pub reviews: usize,
    pub events: usize,
    /// `None` when the observe saw no comments.
    pub comments: Option<CommentCursor>,
}

/// The newest comment update an observe saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommentCursor {
    pub since: Timestamp,
    /// Comments updated at exactly `since`.
    pub seen: Vec<u64>,
}

impl CommentCursor {
    /// The cursor over `comments`, or `None` when there are none.
    pub(crate) fn over(comments: &[super::comments::IssueComment]) -> Option<Self> {
        let since = comments.iter().map(|c| c.updated_at).max()?;
        let seen = comments
            .iter()
            .filter(|c| c.updated_at == since)
            .map(|c| c.id)
            .collect();
        Some(Self { since, seen })
    }

    fn since_param(&self) -> String {
        self.since.at().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Report the first endpoint whose projection moved.
    Watch,
    /// Establish baselines for endpoints never seen; report nothing.
    Prime,
}

/// Conditional fetch, injected so the probe is testable offline.
pub(crate) type Fetch = dyn FnMut(&str, Option<&str>, Duration) -> Result<Conditional, GhError>;

#[derive(Debug, Clone)]
struct Validator {
    etag: Option<String>,
    projection: Value,
}

#[derive(Debug, Clone, Copy)]
enum Projection {
    Pull,
    CheckRuns,
    Status,
    Reviews,
    Events,
    CommentsSince,
}

pub(crate) struct GitHubProbe {
    slug: RepoSlug,
    pr: PullRequestNumber,
    seed: Option<ProbeSeed>,
    check_run_pages: usize,
    validators: HashMap<String, Validator>,
    /// Until when a rate-limit response forbids requests, and why.
    throttled: Option<(Instant, ooda_core::RateLimitScope)>,
    fetch: Box<Fetch>,
}

impl std::fmt::Debug for GitHubProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubProbe")
            .field("slug", &self.slug)
            .field("pr", &self.pr)
            .field("seed", &self.seed)
            .field("endpoints", &self.validators.len())
            .finish_non_exhaustive()
    }
}

impl GitHubProbe {
    pub(crate) fn new(slug: RepoSlug, pr: PullRequestNumber) -> Self {
        Self::with_fetch(slug, pr, Box::new(gh_conditional))
    }

    pub(crate) fn with_fetch(slug: RepoSlug, pr: PullRequestNumber, fetch: Box<Fetch>) -> Self {
        Self {
            slug,
            pr,
            seed: None,
            check_run_pages: 1,
            validators: HashMap::new(),
            throttled: None,
            fetch,
        }
    }

    /// Reposition the endpoints on what the latest observe saw, and
    /// drop the baselines of endpoints it moved away from.
    pub(crate) fn seed(&mut self, seed: ProbeSeed) {
        let live: std::collections::HashSet<String> = self
            .endpoints(&seed)
            .into_iter()
            .map(|(_, path, _)| path)
            .collect();
        self.validators.retain(|path, _| live.contains(path));
        self.seed = Some(seed);
    }

    /// Establish baselines for endpoints the latest seed moved to (a
    /// new head, a list crossing a page). This narrows, not closes, the
    /// window in which a change lands before any baseline exists: one
    /// landing between the observe's read and the prime is absorbed and
    /// surfaces at the wait's deadline.
    pub(crate) fn prime(&mut self, budget: Duration, interrupted: &dyn Fn() -> bool) {
        let _ = self.pass(budget, interrupted, Pass::Prime);
    }

    fn endpoints(&self, seed: &ProbeSeed) -> Vec<(&'static str, String, Projection)> {
        let (slug, pr, head) = (&self.slug, self.pr, &seed.head);
        let mut out = vec![("pull", format!("repos/{slug}/pulls/{pr}"), Projection::Pull)];
        for page in 1..=self.check_run_pages {
            out.push((
                "check-runs",
                format!("repos/{slug}/commits/{head}/check-runs?per_page={PER_PAGE}&page={page}"),
                Projection::CheckRuns,
            ));
        }
        out.push((
            "status",
            format!("repos/{slug}/commits/{head}/status"),
            Projection::Status,
        ));
        out.push((
            "reviews",
            format!(
                "repos/{slug}/pulls/{pr}/reviews?per_page={PER_PAGE}&page={}",
                frontier(seed.reviews)
            ),
            Projection::Reviews,
        ));
        out.push((
            "events",
            format!(
                "repos/{slug}/issues/{pr}/events?per_page={PER_PAGE}&page={}",
                frontier(seed.events)
            ),
            Projection::Events,
        ));
        let since = seed
            .comments
            .as_ref()
            .map(|c| format!("&since={}", c.since_param()))
            .unwrap_or_default();
        out.push((
            "comments",
            format!("repos/{slug}/issues/{pr}/comments?per_page={PER_PAGE}{since}"),
            Projection::CommentsSince,
        ));
        out
    }
}

/// Page the next item of a list holding `count` items lands on.
fn frontier(count: usize) -> usize {
    count / PER_PAGE + 1
}

impl ChangeProbe for GitHubProbe {
    fn poll(&mut self, budget: Duration, interrupted: &dyn Fn() -> bool) -> ProbeResult {
        self.pass(budget, interrupted, Pass::Watch)
    }
}

impl GitHubProbe {
    fn pass(
        &mut self,
        budget: Duration,
        interrupted: &dyn Fn() -> bool,
        pass: Pass,
    ) -> ProbeResult {
        let Some(seed) = self.seed.clone() else {
            return ProbeResult::Unchanged;
        };
        if let Some((until, scope)) = self.throttled {
            let left = until.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                return ProbeResult::Throttled(ooda_core::RateLimitHit {
                    scope,
                    retry_after: ooda_core::PollingInterval::from_secs(left.as_secs().max(1)),
                });
            }
            self.throttled = None;
        }
        let started = Instant::now();
        let mut first_change: Option<&'static str> = None;
        let changed_or = |first: Option<&'static str>, otherwise: ProbeResult| {
            first.map_or(otherwise, |s| {
                ProbeResult::Changed(SingleLineString::new(s))
            })
        };
        for (source, path, projection) in self.endpoints(&seed) {
            if interrupted() {
                break;
            }
            if pass == Pass::Prime
                && (self.validators.contains_key(&path)
                    || matches!(projection, Projection::CommentsSince))
            {
                continue;
            }
            let remaining = budget.saturating_sub(started.elapsed());
            if remaining < MIN_CALL_BUDGET {
                break;
            }
            let call_deadline = CALL_DEADLINE.min(remaining);
            let prior = self.validators.get(&path);
            let response =
                (self.fetch)(&path, prior.and_then(|v| v.etag.as_deref()), call_deadline);
            let (etag, body) = match response {
                Ok(Conditional::NotModified) => continue,
                Ok(Conditional::Fresh { etag, body }) => (etag, body),
                Err(GhError::RateLimited(hit)) => {
                    self.throttled =
                        Some((Instant::now() + hit.retry_after.as_duration(), hit.scope));
                    return changed_or(first_change, ProbeResult::Throttled(hit));
                }
                // The wait's own deadline cut the request short.
                Err(GhError::Timeout) if call_deadline < CALL_DEADLINE => break,
                Err(e) => {
                    return changed_or(
                        first_change,
                        ProbeResult::Failed(SingleLineString::new(format!("{source}: {e}"))),
                    );
                }
            };
            let Ok(value) = serde_json::from_slice::<Value>(&body) else {
                return changed_or(
                    first_change,
                    ProbeResult::Failed(SingleLineString::new(format!(
                        "{source}: response is not JSON"
                    ))),
                );
            };
            if let Projection::CheckRuns = projection
                && path.ends_with("&page=1")
            {
                let total = value
                    .get("total_count")
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(0);
                self.check_run_pages = frontier(total.saturating_sub(1)).max(1);
            }
            let projected = project(projection, &value);
            let moved = match projection {
                Projection::CommentsSince => comment_moved(&projected, seed.comments.as_ref()),
                _ => prior.is_some_and(|v| v.projection != projected),
            };
            self.validators.insert(
                path,
                Validator {
                    etag,
                    projection: projected,
                },
            );
            if moved && pass == Pass::Watch && first_change.is_none() {
                first_change = Some(source);
            }
        }
        changed_or(first_change, ProbeResult::Unchanged)
    }
}

/// Whether the comments page holds anything the observe did not see:
/// an update newer than the cursor, or an unseen comment updated at
/// the cursor's own instant.
fn comment_moved(projected: &Value, cursor: Option<&CommentCursor>) -> bool {
    items(Some(projected)).any(|c| {
        let Some(cursor) = cursor else {
            return true;
        };
        let updated = c
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(|t| Timestamp::parse(t).ok());
        let id = c.get("id").and_then(Value::as_u64);
        match updated {
            Some(t) if t > cursor.since => true,
            Some(t) if t == cursor.since => id.is_none_or(|id| !cursor.seen.contains(&id)),
            Some(_) => false,
            None => true,
        }
    })
}

fn project(projection: Projection, v: &Value) -> Value {
    match projection {
        Projection::Pull => {
            let mut o = pick(
                v,
                &[
                    "state",
                    "draft",
                    "merged",
                    "title",
                    "body",
                    "head.sha",
                    "base.ref",
                    "base.sha",
                    "mergeable",
                    "mergeable_state",
                    "comments",
                    "review_comments",
                    "commits",
                    "updated_at",
                    "auto_merge",
                ],
            );
            for (key, list, field) in [
                ("labels", "labels", "name"),
                ("assignees", "assignees", "login"),
                ("requested_reviewers", "requested_reviewers", "login"),
                ("requested_teams", "requested_teams", "slug"),
            ] {
                o.insert(key.into(), sorted_strings(v.get(list), field));
            }
            Value::Object(o)
        }
        Projection::CheckRuns => {
            let mut runs: Vec<Value> = items(v.get("check_runs"))
                .map(|r| {
                    Value::Object(pick(
                        r,
                        &[
                            "name",
                            "status",
                            "conclusion",
                            "started_at",
                            "completed_at",
                            "app.slug",
                        ],
                    ))
                })
                .collect();
            runs.sort_by_key(Value::to_string);
            Value::Array(runs)
        }
        Projection::Status => {
            let mut statuses: Vec<Value> = items(v.get("statuses"))
                .map(|s| Value::Object(pick(s, &["context", "state"])))
                .collect();
            statuses.sort_by_key(Value::to_string);
            let mut o = pick(v, &["state"]);
            o.insert("statuses".into(), Value::Array(statuses));
            Value::Object(o)
        }
        Projection::Reviews => list(v, &["id", "state", "submitted_at", "commit_id"]),
        Projection::Events => list(v, &["id", "event", "created_at"]),
        Projection::CommentsSince => list(v, &["id", "updated_at"]),
    }
}

fn items(v: Option<&Value>) -> impl Iterator<Item = &Value> {
    v.and_then(Value::as_array).into_iter().flatten()
}

fn list(v: &Value, fields: &[&str]) -> Value {
    Value::Array(
        items(Some(v))
            .map(|item| Value::Object(pick(item, fields)))
            .collect(),
    )
}

/// Copy the dotted `paths` out of `v`; an absent path maps to null.
fn pick(v: &Value, paths: &[&str]) -> Map<String, Value> {
    paths
        .iter()
        .map(|path| {
            let found = path
                .split('.')
                .try_fold(v, |node, key| node.get(key))
                .cloned()
                .unwrap_or(Value::Null);
            ((*path).to_string(), found)
        })
        .collect()
}

fn sorted_strings(v: Option<&Value>, field: &str) -> Value {
    let mut out: Vec<String> = items(v)
        .filter_map(|item| item.get(field).and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    out.sort();
    Value::Array(out.into_iter().map(Value::String).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn seed() -> ProbeSeed {
        ProbeSeed {
            head: GitCommitSha::parse(HEAD).unwrap(),
            reviews: 3,
            events: 250,
            comments: Some(CommentCursor {
                since: Timestamp::parse("2026-10-28T13:07:00Z").unwrap(),
                seen: vec![1],
            }),
        }
    }

    type Responses = Rc<RefCell<HashMap<String, Value>>>;

    /// Serves the current body for each path, 304 when the caller's
    /// validator matches it; records every path requested.
    fn probe(responses: &Responses, requested: &Rc<RefCell<Vec<String>>>) -> GitHubProbe {
        let responses = Rc::clone(responses);
        let requested = Rc::clone(requested);
        let mut p = GitHubProbe::with_fetch(
            RepoSlug::parse("o/r").unwrap(),
            PullRequestNumber::new(7).unwrap(),
            Box::new(move |path, etag, _| {
                requested.borrow_mut().push(path.to_string());
                let resource = path.split('?').next().unwrap_or(path);
                let body = responses
                    .borrow()
                    .get(resource)
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                let tag = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    body.to_string().hash(&mut h);
                    format!("\"{:x}\"", h.finish())
                };
                if etag == Some(tag.as_str()) {
                    Ok(Conditional::NotModified)
                } else {
                    Ok(Conditional::Fresh {
                        etag: Some(tag),
                        body: body.to_string().into_bytes(),
                    })
                }
            }),
        );
        p.seed(seed());
        p
    }

    fn pull(updated_at: &str, open_issues: u64) -> Value {
        json!({
            "state": "open", "head": {"sha": HEAD, "repo": {"open_issues_count": open_issues}},
            "base": {"ref": "master", "sha": "b", "repo": {"pushed_at": updated_at}},
            "labels": [{"name": "b"}, {"name": "a"}], "rebaseable": open_issues.is_multiple_of(2),
            "updated_at": updated_at,
        })
    }

    fn setup() -> (Responses, Rc<RefCell<Vec<String>>>) {
        let responses: Responses = Rc::new(RefCell::new(HashMap::new()));
        responses
            .borrow_mut()
            .insert("repos/o/r/pulls/7".into(), pull("t1", 10));
        (responses, Rc::new(RefCell::new(Vec::new())))
    }

    const BUDGET: Duration = Duration::from_secs(30);

    #[test]
    fn first_pass_establishes_baselines_without_waking() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        assert_eq!(p.poll(BUDGET, &|| false), ProbeResult::Unchanged);
    }

    #[test]
    fn embedded_neighbour_churn_does_not_wake() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        p.poll(BUDGET, &|| false);
        // Repository counters and the flapping `rebaseable` move;
        // the pull request's own fields do not.
        responses
            .borrow_mut()
            .insert("repos/o/r/pulls/7".into(), pull("t1", 11));
        assert_eq!(p.poll(BUDGET, &|| false), ProbeResult::Unchanged);
    }

    #[test]
    fn own_field_change_wakes_naming_the_endpoint() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        p.poll(BUDGET, &|| false);
        responses
            .borrow_mut()
            .insert("repos/o/r/pulls/7".into(), pull("t2", 10));
        assert_eq!(
            p.poll(BUDGET, &|| false),
            ProbeResult::Changed(SingleLineString::new("pull"))
        );
    }

    #[test]
    fn check_run_output_churn_does_not_wake_but_completion_does() {
        let (responses, requested) = setup();
        let runs = |status: &str, title: &str| {
            json!({"total_count": 1, "check_runs": [{
                "name": "build", "status": status, "app": {"slug": "github-actions"},
                "output": {"title": title}, "pull_requests": [{"base": {"sha": title}}]}]})
        };
        let key = format!("repos/o/r/commits/{HEAD}/check-runs");
        responses
            .borrow_mut()
            .insert(key.clone(), runs("in_progress", "a"));
        let mut p = probe(&responses, &requested);
        p.poll(BUDGET, &|| false);
        responses
            .borrow_mut()
            .insert(key.clone(), runs("in_progress", "b"));
        assert_eq!(p.poll(BUDGET, &|| false), ProbeResult::Unchanged);
        responses.borrow_mut().insert(key, runs("completed", "b"));
        assert_eq!(
            p.poll(BUDGET, &|| false),
            ProbeResult::Changed(SingleLineString::new("check-runs"))
        );
    }

    #[test]
    fn lists_are_read_at_their_frontier_page() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        p.poll(BUDGET, &|| false);
        let paths = requested.borrow();
        assert!(
            paths
                .iter()
                .any(|q| q == "repos/o/r/pulls/7/reviews?per_page=100&page=1")
        );
        assert!(
            paths
                .iter()
                .any(|q| q == "repos/o/r/issues/7/events?per_page=100&page=3")
        );
        assert!(
            paths
                .iter()
                .any(|q| q.ends_with("comments?per_page=100&since=2026-10-28T13:07:00Z"))
        );
    }

    #[test]
    fn frontier_is_the_page_the_next_item_lands_on() {
        assert_eq!(frontier(0), 1);
        assert_eq!(frontier(99), 1);
        assert_eq!(frontier(100), 2);
        assert_eq!(frontier(101), 2);
    }

    fn comments_at(items: Value) -> (Responses, Rc<RefCell<Vec<String>>>) {
        let (responses, requested) = setup();
        responses
            .borrow_mut()
            .insert("repos/o/r/issues/7/comments".into(), items);
        (responses, requested)
    }

    #[test]
    fn a_comment_newer_than_the_cursor_wakes() {
        let (responses, requested) =
            comments_at(json!([{"id": 2, "updated_at": "2026-10-28T13:07:30Z"}]));
        let mut p = probe(&responses, &requested);
        assert_eq!(
            p.poll(BUDGET, &|| false),
            ProbeResult::Changed(SingleLineString::new("comments"))
        );
    }

    #[test]
    fn the_comment_the_cursor_already_saw_does_not_wake() {
        let (responses, requested) =
            comments_at(json!([{"id": 1, "updated_at": "2026-10-28T13:07:00Z"}]));
        let mut p = probe(&responses, &requested);
        assert_eq!(p.poll(BUDGET, &|| false), ProbeResult::Unchanged);
    }

    #[test]
    fn an_unseen_comment_at_the_cursor_instant_wakes() {
        let (responses, requested) =
            comments_at(json!([{"id": 3, "updated_at": "2026-10-28T13:07:00Z"}]));
        let mut p = probe(&responses, &requested);
        assert!(matches!(p.poll(BUDGET, &|| false), ProbeResult::Changed(_)));
    }

    #[test]
    fn one_event_moving_two_endpoints_wakes_once() {
        let (responses, requested) = setup();
        let reviews = "repos/o/r/pulls/7/reviews".to_string();
        responses.borrow_mut().insert(reviews.clone(), json!([]));
        let mut p = probe(&responses, &requested);
        p.poll(BUDGET, &|| false);
        responses
            .borrow_mut()
            .insert("repos/o/r/pulls/7".into(), pull("t2", 10));
        responses
            .borrow_mut()
            .insert(reviews, json!([{"id": 5, "state": "COMMENTED"}]));
        assert_eq!(
            p.poll(BUDGET, &|| false),
            ProbeResult::Changed(SingleLineString::new("pull"))
        );
        assert_eq!(
            p.poll(BUDGET, &|| false),
            ProbeResult::Unchanged,
            "the reviews baseline advanced in the same pass"
        );
    }

    #[test]
    fn interrupt_stops_the_pass_before_the_next_request() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        assert_eq!(p.poll(BUDGET, &|| true), ProbeResult::Unchanged);
        assert!(requested.borrow().is_empty());
    }

    #[test]
    fn a_pass_short_of_budget_ends_quietly() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        assert_eq!(
            p.poll(MIN_CALL_BUDGET / 2, &|| false),
            ProbeResult::Unchanged
        );
        assert!(requested.borrow().is_empty());
    }

    #[test]
    fn a_budget_cut_timeout_is_not_a_failure() {
        let mut p = GitHubProbe::with_fetch(
            RepoSlug::parse("o/r").unwrap(),
            PullRequestNumber::new(7).unwrap(),
            Box::new(|_, _, _| Err(GhError::Timeout)),
        );
        p.seed(seed());
        assert_eq!(
            p.poll(Duration::from_secs(3), &|| false),
            ProbeResult::Unchanged
        );
        assert!(matches!(p.poll(BUDGET, &|| false), ProbeResult::Failed(_)));
    }

    #[test]
    fn prime_baselines_new_endpoints_without_waking() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        p.prime(BUDGET, &|| false);
        let primed = requested.borrow().len();
        assert!(primed > 0);
        assert!(
            !requested.borrow().iter().any(|q| q.contains("/comments")),
            "comments compare against the cursor, not a baseline"
        );
        responses
            .borrow_mut()
            .insert("repos/o/r/pulls/7".into(), pull("t2", 10));
        p.prime(BUDGET, &|| false);
        assert_eq!(
            requested.borrow().len(),
            primed,
            "known endpoints are not refetched"
        );
        assert!(matches!(p.poll(BUDGET, &|| false), ProbeResult::Changed(_)));
    }

    #[test]
    fn unseeded_probe_reports_unchanged_without_fetching() {
        let requested = Rc::new(RefCell::new(Vec::new()));
        let r = Rc::clone(&requested);
        let mut p = GitHubProbe::with_fetch(
            RepoSlug::parse("o/r").unwrap(),
            PullRequestNumber::new(7).unwrap(),
            Box::new(move |path, _, _| {
                r.borrow_mut().push(path.to_string());
                Ok(Conditional::NotModified)
            }),
        );
        assert_eq!(p.poll(BUDGET, &|| false), ProbeResult::Unchanged);
        assert!(requested.borrow().is_empty());
    }

    #[test]
    fn rate_limit_throttles_and_other_errors_fail() {
        let calls = Rc::new(RefCell::new(0u32));
        let counted = Rc::clone(&calls);
        let mut throttled = GitHubProbe::with_fetch(
            RepoSlug::parse("o/r").unwrap(),
            PullRequestNumber::new(7).unwrap(),
            Box::new(move |_, _, _| {
                *counted.borrow_mut() += 1;
                Err(GhError::RateLimited(ooda_core::RateLimitHit {
                    scope: ooda_core::RateLimitScope::GitHubSecondary,
                    retry_after: ooda_core::PollingInterval::from_secs(60),
                }))
            }),
        );
        throttled.seed(seed());
        assert!(matches!(
            throttled.poll(BUDGET, &|| false),
            ProbeResult::Throttled(_)
        ));
        assert!(
            matches!(throttled.poll(BUDGET, &|| false), ProbeResult::Throttled(_)),
            "inside retry_after the probe reports the throttle"
        );
        assert_eq!(*calls.borrow(), 1, "without another request");
        let mut failing = GitHubProbe::with_fetch(
            RepoSlug::parse("o/r").unwrap(),
            PullRequestNumber::new(7).unwrap(),
            Box::new(|_, _, _| Err(GhError::NotFound)),
        );
        failing.seed(seed());
        assert!(matches!(
            failing.poll(BUDGET, &|| false),
            ProbeResult::Failed(_)
        ));
    }

    #[test]
    fn exhausted_budget_stops_the_pass() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        assert_eq!(p.poll(Duration::ZERO, &|| false), ProbeResult::Unchanged);
        assert!(requested.borrow().is_empty());
    }
}
