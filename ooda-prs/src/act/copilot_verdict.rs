//! Handoff-prompt composition for reviewer-verdict obligations.
//!
//! Three consumers. The reviews axis frames an address-threads prompt
//! carrying Copilot threads with the latest verdict's assessment and,
//! while that verdict is unanswered, appends the attest step, so the
//! threads are read as instances of the assessment and answered with
//! it. The standalone prompt fires when the verdict is unanswered and
//! no Copilot threads carry it. The escalation prompt hands recorded
//! escalations to a human.

use std::path::Path;

use ooda_core::{HandoffPrompt, NonEmpty, SafeUrl, SingleLineString, Witness};

use crate::ids::PullRequestNumber;
use ooda_core::attest::{CopilotVerdictEntry, VerdictDisposition};

use crate::orient::copilot::{CopilotVerdict, VerdictStance};
use crate::orient::copilot_verdict::Escalation;

/// Append the verdict as the frame Copilot's threads are read
/// through, worded by its standing: an unanswered concern is work, a
/// recorded or escalated one and an approval are context.
pub(crate) fn push_verdict_frame(
    prompt: &mut HandoffPrompt,
    verdict: &CopilotVerdict,
    entry: Option<&CopilotVerdictEntry>,
) {
    prompt.push_heading(3, "Copilot's overall assessment");
    prompt.push_witnesses(NonEmpty::singleton(verdict_witness(verdict)));
    prompt.push_paragraph(match (entry, verdict.stance) {
        (None, VerdictStance::Concern) => "The Copilot threads below are instances of this \
             assessment. Fix the concern it names, not only the anchored lines."
            .to_string(),
        (None, VerdictStance::Approve) => {
            "Copilot recommends approval; the threads below are its remaining findings.".to_string()
        }
        (Some(e), _) if e.disposition == VerdictDisposition::Escalate => format!(
            "Escalated to a human, who decides it: {}. Do not act on the assessment.",
            e.note
        ),
        (Some(e), _) => format!(
            "Recorded as `{}`: {}. Shown for context.",
            e.disposition, e.note
        ),
    });
}

/// Append the attest step shared by the standalone prompt and the
/// address-threads prompt. `heading` carries the step number the
/// caller's numbering assigns.
pub(crate) fn push_verdict_attest_step(
    prompt: &mut HandoffPrompt,
    heading: &str,
    pr: PullRequestNumber,
    verdict: &CopilotVerdict,
    attest_path: Option<&Path>,
) {
    prompt.push_heading(3, heading);
    prompt.push_paragraph(
        "Answer the assessment itself. Disposition `patched`: a commit \
         addresses it — push before recording. `no-change`: nothing in it \
         calls for a change, and the note says why. `escalate`: only a \
         human can decide, and the note says what they must decide.",
    );
    prompt.push_code(
        "bash",
        cli_invocation(pr, verdict.review_id, AGENT_DISPOSITIONS, attest_path),
    );
    prompt.push_paragraph(
        "The entry answers this review only. The loop re-issues this \
         handoff until it exists; a later review raises its own. Never \
         re-record a review already escalated: that decision belongs to \
         a human.",
    );
}

/// Build the standalone resolve-verdict handoff prompt body.
#[must_use]
pub(crate) fn build_resolve_copilot_verdict_prompt(
    pr: PullRequestNumber,
    verdict: &CopilotVerdict,
    attest_path: Option<&Path>,
) -> HandoffPrompt {
    let mut prompt = HandoffPrompt::new("Copilot verdict needs an answer.");
    prompt.push_paragraph(
        "Copilot's latest review states a concern in its overall \
         assessment, and no recorded disposition answers it. The \
         assessment names no file or line. The loop will not re-request \
         review or close out until it is answered.",
    );
    prompt.push_witnesses(NonEmpty::singleton(verdict_witness(verdict)));

    prompt.push_heading(3, "Step 1 — judge the assessment");
    prompt.push_paragraph(
        "Check each claim against the current diff. A claim that holds \
         is a defect: fix it at whatever scope it reaches, and search the \
         working tree for the same class. A claim already addressed, or a \
         request for verification no commit can supply, needs no change.",
    );

    push_verdict_attest_step(
        &mut prompt,
        "Step 2 — record the disposition",
        pr,
        verdict,
        attest_path,
    );
    prompt
}

/// Build the human handoff for escalated verdicts.
#[must_use]
pub(crate) fn build_decide_escalated_prompt(
    pr: PullRequestNumber,
    escalations: &NonEmpty<Escalation>,
    attest_path: Option<&Path>,
) -> HandoffPrompt {
    let mut prompt = HandoffPrompt::new("Escalated Copilot verdict needs a human decision.");
    prompt.push_paragraph(format!(
        "An agent judged {} beyond its authority to decide. Each \
         holds until a human records `patched` or `no-change` for the same \
         review.",
        crate::text::count(escalations.nonzero_len().get(), "Copilot verdict"),
    ));
    let witnesses = escalations.map_ref(|e| {
        let label = match &e.verdict {
            Some(v) => format!("review {} — {}", e.entry.review_id, v.heading),
            None => format!("review {}", e.entry.review_id),
        };
        let body = match &e.verdict {
            Some(v) => format!(
                "Assessment: {}\n\nAgent's escalation: {}",
                v.assessment, e.entry.note
            ),
            None => format!("Agent's escalation: {}", e.entry.note),
        };
        Witness {
            label: SingleLineString::new(label),
            body: body.into(),
            url: e.verdict.as_ref().and_then(|v| SafeUrl::parse(&v.url).ok()),
        }
    });
    prompt.push_witnesses(witnesses);
    prompt.push_heading(3, "Record each decision");
    let invocations: Vec<String> = escalations
        .as_slice()
        .iter()
        .map(|e| cli_invocation(pr, e.entry.review_id, HUMAN_DISPOSITIONS, attest_path))
        .collect();
    prompt.push_code("bash", invocations.join("\n\n"));
    prompt
}

/// Build the human handoff for an attestation file that fails to read.
#[must_use]
pub(crate) fn build_repair_attestation_prompt(attest_path: &Path, error: &str) -> HandoffPrompt {
    let mut prompt = HandoffPrompt::new("Copilot verdict attestation is unreadable.");
    prompt.push_paragraph(format!(
        "`{}` exists but fails to read: {error}. The answers and \
         escalations it records are unknown, and `ooda-attest \
         copilot-verdict` refuses to overwrite it. Repair the file, or \
         remove it to discard every recorded answer.",
        attest_path.display()
    ));
    prompt
}

/// Dispositions an agent may record.
const AGENT_DISPOSITIONS: &str = "patched|no-change|escalate";

/// Dispositions that settle an escalation; escalating again would
/// leave it pending.
const HUMAN_DISPOSITIONS: &str = "patched|no-change";

fn verdict_witness(verdict: &CopilotVerdict) -> Witness {
    Witness {
        label: SingleLineString::new(format!(
            "{} (review {})",
            verdict.heading, verdict.review_id
        )),
        body: if verdict.assessment.trim().is_empty() {
            "(no assessment stated)".to_string()
        } else {
            verdict.assessment.clone()
        }
        .into(),
        url: SafeUrl::parse(&verdict.url).ok(),
    }
}

fn cli_invocation(
    pr: PullRequestNumber,
    review_id: u64,
    dispositions: &str,
    attest_path: Option<&Path>,
) -> String {
    let root = match attest_path.and_then(state_root_from_attest_path) {
        Some(state_root) => state_root.display().to_string(),
        None => "<absolute path to OODA state root; report back if you do not \
                 know it — this invocation was started without --state-root>"
            .to_string(),
    };
    format!(
        "ooda-attest copilot-verdict --pr-id {pr} --state-root {root} \\\n  \
         --review {review_id} --disposition <{dispositions}> \\\n  \
         --note \"<the decision's grounds>\""
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
    use crate::orient::copilot::VerdictStance;
    use ooda_core::attest::{CopilotVerdictEntry, VerdictDisposition};

    fn pr() -> PullRequestNumber {
        PullRequestNumber::parse("1541").unwrap()
    }

    fn verdict() -> CopilotVerdict {
        CopilotVerdict {
            review_id: 5_681_853_593,
            url: "https://github.com/o/r/pull/1541#pullrequestreview-5681853593".into(),
            stance: VerdictStance::Concern,
            heading: "\u{1f535} Needs a closer look".into(),
            assessment: "Stub-only validation leaves provisioning unverified.".into(),
        }
    }

    fn path() -> std::path::PathBuf {
        std::path::PathBuf::from("/state/1541/copilot_verdict_attest.json")
    }

    #[test]
    fn resolve_prompt_carries_assessment_and_keyed_cli() {
        let rendered =
            build_resolve_copilot_verdict_prompt(pr(), &verdict(), Some(&path())).to_string();
        assert!(rendered.contains("Stub-only validation"), "{rendered}");
        assert!(rendered.contains("review 5681853593"), "{rendered}");
        assert!(
            rendered.contains("ooda-attest copilot-verdict --pr-id 1541 --state-root /state"),
            "{rendered}"
        );
        assert!(rendered.contains("--review 5681853593"), "{rendered}");
        assert!(rendered.find("Step 1").unwrap() < rendered.find("Step 2").unwrap());
    }

    #[test]
    fn missing_state_root_renders_placeholder() {
        let rendered = build_resolve_copilot_verdict_prompt(pr(), &verdict(), None).to_string();
        assert!(
            rendered.contains("--state-root <absolute path"),
            "{rendered}"
        );
    }

    #[test]
    fn escalation_prompt_carries_assessment_and_agent_note() {
        let e = Escalation {
            entry: CopilotVerdictEntry {
                review_id: 5_681_853_593,
                disposition: VerdictDisposition::Escalate,
                note: "needs a supervised live setup".into(),
                attested_sha: "0123456789abcdef0123456789abcdef01234567".into(),
                attested_at: chrono::Utc::now(),
            },
            verdict: Some(verdict()),
        };
        let rendered =
            build_decide_escalated_prompt(pr(), &NonEmpty::singleton(e), Some(&path())).to_string();
        assert!(rendered.contains("Stub-only validation"), "{rendered}");
        assert!(
            rendered.contains("needs a supervised live setup"),
            "{rendered}"
        );
        assert!(rendered.contains("--review 5681853593"), "{rendered}");
    }
}
