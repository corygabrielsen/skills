//! Act stage: execute Full actions directly, sleep on Wait actions.
//!
//! Decide has already routed Agent and Human actions to Halt — they
//! never reach act. Anything Action that arrives here is either
//! Full (we run it) or Wait (we sleep next_poll_seconds and return).

use std::thread;

use crate::decide::action::{Action, ActionKind, Automation};
use crate::ids::{PullRequestNumber, RepoSlug};
use crate::observe::github::gh::{gh_run, GhError};
use crate::orient::copilot::COPILOT_REVIEWER_LOGIN;
use crate::orient::state::WIP_LABEL;

#[derive(Debug)]
pub enum ActError {
    /// Decide guarantees act() only sees Full or Wait actions; an
    /// Agent or Human action here is a programmer error.
    UnsupportedAutomation,
    /// `gh` subprocess failed for a Full action.
    Gh(GhError),
}

impl std::fmt::Display for ActError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAutomation => {
                write!(f, "act received an Agent/Human action — decide must halt those")
            }
            Self::Gh(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ActError {}

impl From<GhError> for ActError {
    fn from(e: GhError) -> Self {
        Self::Gh(e)
    }
}

/// Execute (or wait for) one action. Returns Ok on success;
/// caller's loop re-iterates after this returns.
pub fn act(
    action: &Action,
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<(), ActError> {
    match action.automation {
        Automation::Full => run_full(&action.kind, slug, pr),
        Automation::Wait { interval } => {
            thread::sleep(interval);
            Ok(())
        }
        Automation::Agent | Automation::Human => Err(ActError::UnsupportedAutomation),
    }
}

fn run_full(
    kind: &ActionKind,
    slug: &RepoSlug,
    pr: PullRequestNumber,
) -> Result<(), ActError> {
    // gh_run takes &[&str], so the formatted strings need backing
    // storage on the stack for the duration of the call.
    let pr_s = pr.to_string();
    let slug_s = slug.to_string();
    match kind {
        ActionKind::MarkReady => gh_run(&["pr", "ready", &pr_s, "-R", &slug_s])?,
        ActionKind::RemoveWipLabel => gh_run(&[
            "pr", "edit", &pr_s, "-R", &slug_s, "--remove-label", WIP_LABEL,
        ])?,
        ActionKind::RerequestCopilot => {
            let path = format!("repos/{slug}/pulls/{pr}/requested_reviewers");
            let reviewer = format!("reviewers[]={COPILOT_REVIEWER_LOGIN}");
            gh_run(&["api", &path, "--method", "POST", "-f", &reviewer])?;
        }
        _ => return Err(ActError::UnsupportedAutomation),
    }
    Ok(())
}
