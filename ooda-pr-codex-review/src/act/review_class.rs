//! Handoff-prompt composition for the review-class attestation gate.
//!
//! Two consumers. The reviews axis appends the attest step to every
//! address-threads prompt so the sweep and its witness travel as one
//! instruction. The standalone prompt fires when threads are all
//! resolved but no attestation is newer than the latest thread —
//! the state an agent leaves behind when it fixes the instances,
//! resolves the anchors, and skips the class.

use std::path::Path;

use ooda_core::attest::ReviewClassAttestation;
use ooda_core::{HandoffPrompt, NonEmpty, SingleLineString};

use crate::ids::PullRequestNumber;
use crate::orient::review_class::ReviewClass;

/// Build the standalone attest-review-class handoff prompt body.
#[must_use]
pub(crate) fn build_attest_review_class_prompt(
    pr: PullRequestNumber,
    review_class: &ReviewClass,
    attest_path: Option<&Path>,
) -> HandoffPrompt {
    let mut prompt = HandoffPrompt::new("Review-class sweep not attested.");

    let (fresh_count, latest, prior) = match review_class {
        ReviewClass::Fresh {
            latest_thread_at,
            fresh_thread_count,
            prior,
        } => (*fresh_thread_count, Some(*latest_thread_at), prior.as_ref()),
        ReviewClass::NoThreads | ReviewClass::Attested { .. } => (0, None, None),
    };

    prompt.push_paragraph(format!(
        "Every review thread on this PR is resolved, but no attestation \
         records that the issue classes behind them were swept across the \
         working tree{}. Resolving a thread clears the reviewer's anchor; it \
         does not show the class is gone. The loop will not re-request \
         review or close out until the sweep is attested.",
        match latest {
            Some(t) => format!(
                " ({} since the last attestation, newest at {t})",
                crate::text::count(fresh_count, "thread"),
            ),
            None => String::new(),
        },
    ));

    if let Some(prior) = prior {
        push_prior_classes(&mut prompt, prior);
    }

    prompt.push_heading(3, "Step 1 — sweep each class");
    prompt.push_paragraph(
        "For each thread raised since the last attestation, name the issue \
         class it belongs to. Search the whole working tree for every other \
         instance of that class, not only the lines the reviewer anchored. \
         Fix each instance. Commit and push before attesting; the \
         attestation records HEAD.",
    );

    push_attest_step(&mut prompt, "Step 2 — attest the sweep", pr, attest_path);
    prompt
}

/// Append the attest step shared by the standalone prompt and the
/// address-threads prompt. `heading` carries the step number the
/// caller's numbering assigns.
pub(crate) fn push_attest_step(
    prompt: &mut HandoffPrompt,
    heading: &str,
    pr: PullRequestNumber,
    attest_path: Option<&Path>,
) {
    prompt.push_heading(3, heading);
    prompt.push_paragraph(
        "One `--class` per distinct issue class the reviewers raised, paired \
         positionally with one `--sites` list naming every `path:line` where \
         that class was fixed or judged not to apply. Enumerate by searching \
         the working tree, not by copying the reviewer's anchors: a class \
         whose site list is exactly the anchors the reviewer flagged is the \
         instance fix this gate exists to reject.",
    );
    prompt.push_code("bash", cli_invocation(pr, attest_path));
    prompt.push_paragraph(
        "The binary reads HEAD and writes the attestation atomically; it \
         refuses a class with no sites. The loop re-issues this handoff until \
         the attestation is newer than every review thread. A later review \
         round that raises a class already listed here means the sweep was \
         incomplete; widen it rather than re-attesting the same sites.",
    );
}

/// Append the classes recorded by the prior attestation so the agent
/// can see what was already claimed swept — the repeat signal.
pub(crate) fn push_prior_classes(prompt: &mut HandoffPrompt, prior: &ReviewClassAttestation) {
    prompt.push_heading(3, "Classes attested last round");
    prompt.push_paragraph(format!(
        "Attested at {} against {}. If a new thread falls in one of these \
         classes, the earlier sweep missed instances.",
        prior.attested_at,
        prior.attested_sha.chars().take(7).collect::<String>(),
    ));
    let items: Vec<SingleLineString> = prior
        .classes
        .iter()
        .map(|c| {
            let sites: Vec<String> = c.sites.iter().map(ToString::to_string).collect();
            SingleLineString::new(format!("{} — {}", c.class, sites.join(", ")))
        })
        .collect();
    if let Some(list) = NonEmpty::try_from_vec(items) {
        prompt.push_numbered_list(list);
    }
}

fn cli_invocation(pr: PullRequestNumber, attest_path: Option<&Path>) -> String {
    let root = match attest_path.and_then(state_root_from_attest_path) {
        Some(state_root) => state_root.display().to_string(),
        None => "<absolute path to OODA state root; report back if you do not \
                 know it — this invocation was started without --state-root>"
            .to_string(),
    };
    format!(
        "ooda-attest review-class --pr-id {pr} --state-root {root} \\\n  \
         --class \"<issue class>\" --sites <path:line>,<path:line> \\\n  \
         --class \"<another class>\" --sites <path:line>"
    )
}

fn state_root_from_attest_path(path: &Path) -> Option<std::path::PathBuf> {
    let pr_dir = path.parent()?;
    let state_root = pr_dir.parent()?;
    Some(state_root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use ooda_core::attest::{REVIEW_CLASS_SCHEMA_VERSION, ReviewClassEntry, ReviewSite};

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("753").unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn prior() -> ReviewClassAttestation {
        ReviewClassAttestation {
            attested_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            attested_at: at("2026-05-01T10:00:00Z"),
            version: REVIEW_CLASS_SCHEMA_VERSION,
            classes: vec![ReviewClassEntry {
                class: "unwrap in library code".into(),
                sites: vec![
                    ReviewSite {
                        path: "src/a.rs".into(),
                        line: 12,
                    },
                    ReviewSite {
                        path: "src/b.rs".into(),
                        line: 40,
                    },
                ],
            }],
        }
    }

    fn fresh(prior: Option<ReviewClassAttestation>) -> ReviewClass {
        ReviewClass::Fresh {
            latest_thread_at: at("2026-05-02T10:00:00Z"),
            fresh_thread_count: 3,
            prior,
        }
    }

    #[test]
    fn prompt_starts_with_headline_and_names_thread_count() {
        let path = std::path::PathBuf::from("/state/753/review_class_attest.json");
        let s = build_attest_review_class_prompt(pr(), &fresh(None), Some(&path)).to_string();
        assert!(s.starts_with("# Review-class sweep not attested."), "{s}");
        assert!(s.contains("3 threads since the last attestation"), "{s}");
        assert!(s.contains("2026-05-02"), "{s}");
    }

    #[test]
    fn prompt_includes_literal_ooda_attest_command_with_state_root() {
        let path = std::path::PathBuf::from("/state/753/review_class_attest.json");
        let s = build_attest_review_class_prompt(pr(), &fresh(None), Some(&path)).to_string();
        assert!(
            s.contains("ooda-attest review-class --pr-id 753 --state-root /state"),
            "{s}",
        );
        assert!(s.contains("--class"), "{s}");
        assert!(s.contains("--sites"), "{s}");
    }

    #[test]
    fn prompt_falls_back_to_placeholder_when_no_attest_path() {
        let s = build_attest_review_class_prompt(pr(), &fresh(None), None).to_string();
        assert!(s.contains("report back if you do not know it"), "{s}");
    }

    #[test]
    fn prompt_lists_prior_classes_with_sites() {
        let s = build_attest_review_class_prompt(pr(), &fresh(Some(prior())), None).to_string();
        assert!(s.contains("Classes attested last round"), "{s}");
        assert!(
            s.contains("unwrap in library code — src/a.rs:12, src/b.rs:40"),
            "{s}",
        );
        assert!(s.contains("0123456"), "{s}");
    }

    #[test]
    fn prompt_omits_prior_section_when_never_attested() {
        let s = build_attest_review_class_prompt(pr(), &fresh(None), None).to_string();
        assert!(!s.contains("Classes attested last round"), "{s}");
    }

    #[test]
    fn prompt_orders_sweep_before_attest() {
        let s = build_attest_review_class_prompt(pr(), &fresh(None), None).to_string();
        let sweep = s.find("Step 1").expect("step 1");
        let attest = s.find("Step 2").expect("step 2");
        let cmd = s.find("ooda-attest review-class").expect("command");
        assert!(sweep < attest && attest < cmd);
    }

    #[test]
    fn attest_step_warns_against_copying_reviewer_anchors() {
        let mut p = HandoffPrompt::new("h");
        push_attest_step(&mut p, "Step 3 — attest", pr(), None);
        let s = p.to_string();
        assert!(s.contains("### Step 3 — attest"), "{s}");
        assert!(
            s.contains("exactly the anchors the reviewer flagged"),
            "{s}"
        );
        assert!(s.contains("refuses a class with no sites"), "{s}");
    }
}
