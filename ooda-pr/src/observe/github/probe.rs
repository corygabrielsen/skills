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
//! - **Comments by recency**: issue comments are read `since` the
//!   last observe began, so a new or edited comment is a non-empty
//!   page.
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

/// What the last observe saw, which positions every endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeSeed {
    pub head: GitCommitSha,
    pub reviews: usize,
    pub events: usize,
    /// When that observe began; comments are read from here.
    pub observed_from: Timestamp,
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
            fetch,
        }
    }

    /// Reposition the endpoints on what the latest observe saw.
    pub(crate) fn seed(&mut self, seed: ProbeSeed) {
        self.seed = Some(seed);
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
        out.push((
            "comments",
            format!(
                "repos/{slug}/issues/{pr}/comments?per_page={PER_PAGE}&since={}",
                seed.observed_from.at().format("%Y-%m-%dT%H:%M:%SZ")
            ),
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
    fn poll(&mut self, budget: Duration) -> ProbeResult {
        let Some(seed) = self.seed.clone() else {
            return ProbeResult::Unchanged;
        };
        let started = Instant::now();
        for (source, path, projection) in self.endpoints(&seed) {
            let remaining = budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return ProbeResult::Unchanged;
            }
            let prior = self.validators.get(&path);
            let response = (self.fetch)(
                &path,
                prior.and_then(|v| v.etag.as_deref()),
                CALL_DEADLINE.min(remaining),
            );
            let (etag, body) = match response {
                Ok(Conditional::NotModified) => continue,
                Ok(Conditional::Fresh { etag, body }) => (etag, body),
                Err(GhError::RateLimited(hit)) => return ProbeResult::Throttled(hit),
                Err(e) => {
                    return ProbeResult::Failed(SingleLineString::new(format!("{source}: {e}")));
                }
            };
            let Ok(value) = serde_json::from_slice::<Value>(&body) else {
                return ProbeResult::Failed(SingleLineString::new(format!(
                    "{source}: response is not JSON"
                )));
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
            let changed = match projection {
                Projection::CommentsSince => projected.as_array().is_some_and(|a| !a.is_empty()),
                _ => prior.is_some_and(|v| v.projection != projected),
            };
            self.validators.insert(
                path,
                Validator {
                    etag,
                    projection: projected,
                },
            );
            if changed {
                return ProbeResult::Changed(SingleLineString::new(source));
            }
        }
        ProbeResult::Unchanged
    }
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
            observed_from: Timestamp::parse("2026-10-28T13:07:00Z").unwrap(),
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
                let body = responses
                    .borrow()
                    .iter()
                    .find(|(prefix, _)| path.starts_with(prefix.as_str()))
                    .map_or(json!([]), |(_, v)| v.clone());
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
        assert_eq!(p.poll(BUDGET), ProbeResult::Unchanged);
    }

    #[test]
    fn embedded_neighbour_churn_does_not_wake() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        p.poll(BUDGET);
        // Repository counters and the flapping `rebaseable` move;
        // the pull request's own fields do not.
        responses
            .borrow_mut()
            .insert("repos/o/r/pulls/7".into(), pull("t1", 11));
        assert_eq!(p.poll(BUDGET), ProbeResult::Unchanged);
    }

    #[test]
    fn own_field_change_wakes_naming_the_endpoint() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        p.poll(BUDGET);
        responses
            .borrow_mut()
            .insert("repos/o/r/pulls/7".into(), pull("t2", 10));
        assert_eq!(
            p.poll(BUDGET),
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
        p.poll(BUDGET);
        responses
            .borrow_mut()
            .insert(key.clone(), runs("in_progress", "b"));
        assert_eq!(p.poll(BUDGET), ProbeResult::Unchanged);
        responses.borrow_mut().insert(key, runs("completed", "b"));
        assert_eq!(
            p.poll(BUDGET),
            ProbeResult::Changed(SingleLineString::new("check-runs"))
        );
    }

    #[test]
    fn lists_are_read_at_their_frontier_page() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        p.poll(BUDGET);
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

    #[test]
    fn a_comment_since_the_observe_wakes_on_first_sight() {
        let (responses, requested) = setup();
        responses.borrow_mut().insert(
            "repos/o/r/issues/7/comments".into(),
            json!([{"id": 1, "updated_at": "2026-10-28T13:07:30Z", "body": "x"}]),
        );
        let mut p = probe(&responses, &requested);
        assert_eq!(
            p.poll(BUDGET),
            ProbeResult::Changed(SingleLineString::new("comments"))
        );
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
        assert_eq!(p.poll(BUDGET), ProbeResult::Unchanged);
        assert!(requested.borrow().is_empty());
    }

    #[test]
    fn rate_limit_throttles_and_other_errors_fail() {
        let mut throttled = GitHubProbe::with_fetch(
            RepoSlug::parse("o/r").unwrap(),
            PullRequestNumber::new(7).unwrap(),
            Box::new(|_, _, _| {
                Err(GhError::RateLimited(ooda_core::RateLimitHit {
                    scope: ooda_core::RateLimitScope::GitHubSecondary,
                    retry_after: ooda_core::PollingInterval::from_secs(60),
                }))
            }),
        );
        throttled.seed(seed());
        assert!(matches!(throttled.poll(BUDGET), ProbeResult::Throttled(_)));
        let mut failing = GitHubProbe::with_fetch(
            RepoSlug::parse("o/r").unwrap(),
            PullRequestNumber::new(7).unwrap(),
            Box::new(|_, _, _| Err(GhError::NotFound)),
        );
        failing.seed(seed());
        assert!(matches!(failing.poll(BUDGET), ProbeResult::Failed(_)));
    }

    #[test]
    fn exhausted_budget_stops_the_pass() {
        let (responses, requested) = setup();
        let mut p = probe(&responses, &requested);
        assert_eq!(p.poll(Duration::ZERO), ProbeResult::Unchanged);
        assert!(requested.borrow().is_empty());
    }
}
