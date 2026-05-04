//! The convergence loop: observe → decide → act → halt.
//!
//! Domain-agnostic. Communicates with fitness skills via subprocess
//! (JSON on stdout) and with hooks via JSONL on stdin. Never
//! interprets action kinds, blocker strings, or terminal states.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::fitness;
use crate::halt::{ActionSummary, ErrorCause, HaltReport, HaltStatus, IterLog};
use crate::hook::Hook;
use crate::protocol::{Action, Automation, FitnessReport, TargetEffect};
use crate::session::Session;
use crate::session::now_iso;

const MAX_POLLS_PER_ITER: u32 = 20;
const POST_FULL_REOBSERVE_MS: u64 = 15_000;

pub struct ConvergeOpts {
    pub fitness_argv: Vec<String>,
    pub max_iter: u32,
    pub session_id: String,
    pub resume_cmd: Vec<String>,
    pub hook_cmd: Option<String>,
    pub verbose: bool,
}

fn pick_action(actions: &[Action]) -> Option<&Action> {
    actions
        .iter()
        .find(|a| a.target_effect != TargetEffect::Neutral)
}

fn target_reached(report: &FitnessReport) -> bool {
    report.score >= report.target
}

fn action_summary(action: &Action) -> ActionSummary {
    ActionSummary {
        kind: action.kind.clone(),
        automation: action.automation,
    }
}

/// Stable stringification for iteration-key dedup.
fn stable_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("\"{}\"", s.replace('"', "\\\"")),
        serde_json::Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(stable_json).collect();
            format!("[{}]", inner.join(","))
        }
        serde_json::Value::Object(map) => {
            let mut sorted: BTreeMap<&String, &serde_json::Value> = BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k, v);
            }
            let inner: Vec<String> = sorted
                .iter()
                .map(|(k, v)| format!("\"{}\":{}", k, stable_json(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

/// Iteration key: identity of a logical state. Same key = same iteration.
fn iter_key(action: &Action, report: &FitnessReport) -> String {
    let mut blockers: Vec<String> = report.blockers.as_deref().unwrap_or_default().to_vec();
    blockers.sort();
    let blocker_str = blockers.join("|");

    let activity = report
        .activity_state
        .as_ref()
        .map(|m| stable_json(&serde_json::Value::Object(m.clone())))
        .unwrap_or_else(|| "{}".to_string());

    let type_digest = action
        .r#type
        .as_ref()
        .map(stable_json)
        .unwrap_or_else(|| "null".to_string());

    format!(
        "{}\0{}\0{}\0{}",
        action.kind, type_digest, blocker_str, activity
    )
}

fn trace(verbose: bool, msg: &str) {
    if verbose {
        eprintln!("converge: {msg}");
    }
}

fn make_halt(
    status: HaltStatus,
    session: &Session,
    opts: &ConvergeOpts,
    iter: u32,
    score: Option<f64>,
) -> HaltReport {
    HaltReport {
        stage: "final".to_string(),
        status,
        timestamp: now_iso(),
        session_id: opts.session_id.clone(),
        resume_cmd: opts.resume_cmd.clone(),
        iterations: iter,
        final_score: score,
        structural_blockers: None,
        action: None,
        terminal: None,
        cause: None,
        history: session.history.clone(),
    }
}

pub fn converge(opts: ConvergeOpts, cancelled: &AtomicBool) -> Result<HaltReport, String> {
    let mut session = Session::open(&opts.session_id)?;

    let mut hook = opts
        .hook_cmd
        .as_deref()
        .map(Hook::spawn)
        .transpose()
        .map_err(|e| format!("cannot spawn hook: {e}"))?;

    let start_iter = session.history.len() as u32 + 1;
    let mut iter = start_iter - 1;
    let mut last_score: Option<f64> = session.history.last().map(|h| h.score);
    let mut last_report: Option<FitnessReport> = None;
    let mut current_key: Option<String> = None;
    let max_polls = opts.max_iter * MAX_POLLS_PER_ITER;
    let mut poll_count: u32 = 0;

    session.write_in_progress(&opts.session_id, &opts.resume_cmd)?;

    let finalize = |halt: HaltReport,
                    session: &Session,
                    hook: &mut Option<Hook>,
                    last_report: &Option<FitnessReport>|
     -> Result<HaltReport, String> {
        session.write_halt(&halt)?;
        let score_str = halt
            .final_score
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".to_string());
        eprintln!(
            "halt {:?} iter {} score={}",
            halt.status, halt.iterations, score_str
        );

        if halt.status == HaltStatus::AgentNeeded
            || (halt.status == HaltStatus::Success
                && halt
                    .structural_blockers
                    .as_ref()
                    .is_some_and(|b| !b.is_empty()))
        {
            eprintln!("to resume: {}", halt.resume_cmd.join(" "));
        }

        if let Some(h) = hook.as_mut() {
            h.send_halt(&halt, last_report.as_ref());
        }
        if let Some(h) = hook.take() {
            h.finish();
        }
        session.release();
        Ok(halt)
    };

    while poll_count < max_polls {
        if cancelled.load(Ordering::Relaxed) {
            let halt = make_halt(HaltStatus::Cancelled, &session, &opts, iter, last_score);
            return finalize(halt, &session, &mut hook, &last_report);
        }
        poll_count += 1;

        trace(opts.verbose, &format!("poll {poll_count} (iter {iter})"));

        // Observe.
        let report = match fitness::invoke(&opts.fitness_argv, cancelled) {
            Ok(r) => r,
            Err(fitness::FitnessError::Cancelled) => {
                let halt = make_halt(HaltStatus::Cancelled, &session, &opts, iter, last_score);
                return finalize(halt, &session, &mut hook, &last_report);
            }
            Err(fitness::FitnessError::Permanent(msg)) => {
                let mut halt =
                    make_halt(HaltStatus::FitnessUnavailable, &session, &opts, iter, None);
                halt.cause = Some(ErrorCause {
                    source: "fitness".to_string(),
                    message: msg.clone(),
                    stderr: Some(msg),
                    action_kind: None,
                });
                return finalize(halt, &session, &mut hook, &last_report);
            }
            Err(fitness::FitnessError::Transient(msg)) => {
                // Should not reach here (invoke retries internally), but handle.
                let mut halt =
                    make_halt(HaltStatus::FitnessUnavailable, &session, &opts, iter, None);
                halt.cause = Some(ErrorCause {
                    source: "fitness".to_string(),
                    message: msg.clone(),
                    stderr: Some(msg),
                    action_kind: None,
                });
                return finalize(halt, &session, &mut hook, &last_report);
            }
        };

        last_score = Some(report.score);
        last_report = Some(report.clone());

        let action = pick_action(&report.actions);

        // Decide.
        if target_reached(&report) {
            let structural = report
                .blocker_split
                .as_ref()
                .map(|b| b.structural.clone())
                .unwrap_or_default();
            let mut halt = make_halt(
                HaltStatus::Success,
                &session,
                &opts,
                iter,
                Some(report.score),
            );
            if !structural.is_empty() {
                halt.structural_blockers = Some(structural);
            }
            return finalize(halt, &session, &mut hook, &last_report);
        }

        if report.terminal.is_some() {
            let mut halt = make_halt(
                HaltStatus::Terminal,
                &session,
                &opts,
                iter,
                Some(report.score),
            );
            halt.terminal = report
                .terminal
                .as_ref()
                .map(|t| serde_json::to_value(t).unwrap_or_default());
            return finalize(halt, &session, &mut hook, &last_report);
        }

        let action = match action {
            None => {
                let halt = make_halt(
                    HaltStatus::Stalled,
                    &session,
                    &opts,
                    iter,
                    Some(report.score),
                );
                return finalize(halt, &session, &mut hook, &last_report);
            }
            Some(a) => a,
        };

        // Iteration key dedup.
        let new_key = iter_key(action, &report);
        let is_new = current_key.as_ref() != Some(&new_key);

        if is_new {
            current_key = Some(new_key);
            iter += 1;

            if iter >= start_iter + opts.max_iter {
                let halt = make_halt(
                    HaltStatus::Timeout,
                    &session,
                    &opts,
                    iter - 1,
                    Some(report.score),
                );
                return finalize(halt, &session, &mut hook, &last_report);
            }

            let log_entry = IterLog {
                iter,
                score: report.score,
                action_summary: action_summary(action),
            };
            session.append_history(log_entry)?;

            eprintln!(
                "iter {} score={} action={} ({:?})",
                iter, report.score, action.kind, action.automation
            );

            // Send iteration event to hook (skip agent/human — halt fires next).
            if action.automation != Automation::Agent
                && action.automation != Automation::Human
                && let Some(h) = hook.as_mut()
            {
                h.send_iteration(iter, &report, action);
            }
        }

        // Act.
        match action.automation {
            Automation::Full => {
                if !is_new {
                    interruptible_sleep(POST_FULL_REOBSERVE_MS, cancelled)?;
                    continue;
                }
                let execute = action.execute.as_deref().unwrap_or_default();
                if execute.is_empty() {
                    let mut halt =
                        make_halt(HaltStatus::Error, &session, &opts, iter, Some(report.score));
                    halt.cause = Some(ErrorCause {
                        source: "execute".to_string(),
                        message: "full action has empty execute argv".to_string(),
                        stderr: None,
                        action_kind: Some(action.kind.clone()),
                    });
                    return finalize(halt, &session, &mut hook, &last_report);
                }
                let (cmd, args) = execute.split_first().unwrap();
                let result = Command::new(cmd)
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .status();
                match result {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        let mut halt =
                            make_halt(HaltStatus::Error, &session, &opts, iter, Some(report.score));
                        halt.cause = Some(ErrorCause {
                            source: "execute".to_string(),
                            message: format!(
                                "action {} exited {}",
                                action.kind,
                                status.code().unwrap_or(-1)
                            ),
                            stderr: None,
                            action_kind: Some(action.kind.clone()),
                        });
                        return finalize(halt, &session, &mut hook, &last_report);
                    }
                    Err(e) => {
                        let mut halt =
                            make_halt(HaltStatus::Error, &session, &opts, iter, Some(report.score));
                        halt.cause = Some(ErrorCause {
                            source: "execute".to_string(),
                            message: format!("spawn failed: {e}"),
                            stderr: None,
                            action_kind: Some(action.kind.clone()),
                        });
                        return finalize(halt, &session, &mut hook, &last_report);
                    }
                }
            }
            Automation::Agent => {
                let mut halt = make_halt(
                    HaltStatus::AgentNeeded,
                    &session,
                    &opts,
                    iter,
                    Some(report.score),
                );
                halt.action = Some(action.clone());
                return finalize(halt, &session, &mut hook, &last_report);
            }
            Automation::Wait => {
                let secs = action.next_poll_seconds.unwrap_or(60.0);
                let ms = (secs * 1000.0) as u64;
                interruptible_sleep(ms, cancelled)?;
            }
            Automation::Human => {
                let mut halt =
                    make_halt(HaltStatus::Hil, &session, &opts, iter, Some(report.score));
                halt.action = Some(action.clone());
                return finalize(halt, &session, &mut hook, &last_report);
            }
        }
    }

    // Poll cap exhausted.
    let halt = make_halt(HaltStatus::Timeout, &session, &opts, iter, last_score);
    finalize(halt, &session, &mut hook, &last_report)
}

fn interruptible_sleep(ms: u64, cancelled: &AtomicBool) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        if cancelled.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_action(kind: &str, effect: TargetEffect) -> Action {
        Action {
            kind: kind.to_string(),
            description: "test".to_string(),
            automation: Automation::Full,
            target_effect: effect,
            r#type: None,
            execute: None,
            context: None,
            next_poll_seconds: None,
            timeout_seconds: None,
        }
    }

    fn make_report(score: f64, target: f64, actions: Vec<Action>) -> FitnessReport {
        FitnessReport {
            score,
            target,
            actions,
            status: None,
            score_display: None,
            target_display: None,
            score_emoji: None,
            score_label: None,
            target_label: None,
            axes: None,
            snapshot: None,
            notes: None,
            blockers: None,
            blocker_split: None,
            activity_state: None,
            terminal: None,
        }
    }

    // -- pick_action --

    #[test]
    fn pick_action_skips_neutral() {
        let actions = vec![
            make_action("noop", TargetEffect::Neutral),
            make_action("rebase", TargetEffect::Advances),
        ];
        let picked = pick_action(&actions).unwrap();
        assert_eq!(picked.kind, "rebase");
    }

    #[test]
    fn pick_action_returns_first_non_neutral() {
        let actions = vec![
            make_action("neutral1", TargetEffect::Neutral),
            make_action("blocker", TargetEffect::Blocks),
            make_action("advancer", TargetEffect::Advances),
        ];
        let picked = pick_action(&actions).unwrap();
        assert_eq!(picked.kind, "blocker");
    }

    #[test]
    fn pick_action_none_when_all_neutral() {
        let actions = vec![
            make_action("a", TargetEffect::Neutral),
            make_action("b", TargetEffect::Neutral),
        ];
        assert!(pick_action(&actions).is_none());
    }

    #[test]
    fn pick_action_none_when_empty() {
        assert!(pick_action(&[]).is_none());
    }

    // -- target_reached --

    #[test]
    fn target_reached_true_when_score_equals_target() {
        let report = make_report(1.0, 1.0, vec![]);
        assert!(target_reached(&report));
    }

    #[test]
    fn target_reached_true_when_score_exceeds_target() {
        let report = make_report(1.5, 1.0, vec![]);
        assert!(target_reached(&report));
    }

    #[test]
    fn target_reached_false_when_score_below_target() {
        let report = make_report(0.5, 1.0, vec![]);
        assert!(!target_reached(&report));
    }

    // -- stable_json --

    #[test]
    fn stable_json_null() {
        assert_eq!(stable_json(&json!(null)), "null");
    }

    #[test]
    fn stable_json_bool() {
        assert_eq!(stable_json(&json!(true)), "true");
        assert_eq!(stable_json(&json!(false)), "false");
    }

    #[test]
    fn stable_json_number() {
        assert_eq!(stable_json(&json!(42)), "42");
    }

    #[test]
    fn stable_json_string() {
        assert_eq!(stable_json(&json!("hello")), r#""hello""#);
    }

    #[test]
    fn stable_json_string_with_quotes() {
        assert_eq!(stable_json(&json!("say \"hi\"")), r#""say \"hi\"""#);
    }

    #[test]
    fn stable_json_sorted_keys() {
        let obj = json!({"z": 1, "a": 2, "m": 3});
        assert_eq!(stable_json(&obj), r#"{"a":2,"m":3,"z":1}"#);
    }

    #[test]
    fn stable_json_nested_object_and_array() {
        let val = json!({"b": [1, {"x": true}], "a": null});
        assert_eq!(stable_json(&val), r#"{"a":null,"b":[1,{"x":true}]}"#);
    }

    #[test]
    fn stable_json_empty_array() {
        assert_eq!(stable_json(&json!([])), "[]");
    }

    #[test]
    fn stable_json_empty_object() {
        assert_eq!(stable_json(&json!({})), "{}");
    }

    // -- iter_key --

    #[test]
    fn iter_key_differs_for_different_blockers() {
        let action = make_action("rebase", TargetEffect::Advances);
        let mut r1 = make_report(0.5, 1.0, vec![]);
        r1.blockers = Some(vec!["ci-red".to_string()]);
        let mut r2 = make_report(0.5, 1.0, vec![]);
        r2.blockers = Some(vec!["review-pending".to_string()]);

        assert_ne!(iter_key(&action, &r1), iter_key(&action, &r2));
    }

    #[test]
    fn iter_key_same_for_same_state() {
        let action = make_action("rebase", TargetEffect::Advances);
        let mut report = make_report(0.5, 1.0, vec![]);
        report.blockers = Some(vec!["ci-red".to_string()]);

        assert_eq!(iter_key(&action, &report), iter_key(&action, &report));
    }

    #[test]
    fn iter_key_excludes_score() {
        let action = make_action("rebase", TargetEffect::Advances);
        let r1 = make_report(0.3, 1.0, vec![]);
        let r2 = make_report(0.7, 1.0, vec![]);

        assert_eq!(iter_key(&action, &r1), iter_key(&action, &r2));
    }

    #[test]
    fn iter_key_differs_for_different_action_kind() {
        let a1 = make_action("rebase", TargetEffect::Advances);
        let a2 = make_action("merge", TargetEffect::Advances);
        let report = make_report(0.5, 1.0, vec![]);

        assert_ne!(iter_key(&a1, &report), iter_key(&a2, &report));
    }

    #[test]
    fn iter_key_differs_for_different_activity_state() {
        let action = make_action("wait", TargetEffect::Advances);
        let mut r1 = make_report(0.5, 1.0, vec![]);
        let mut m1 = serde_json::Map::new();
        m1.insert("run_id".to_string(), json!("aaa"));
        r1.activity_state = Some(m1);

        let mut r2 = make_report(0.5, 1.0, vec![]);
        let mut m2 = serde_json::Map::new();
        m2.insert("run_id".to_string(), json!("bbb"));
        r2.activity_state = Some(m2);

        assert_ne!(iter_key(&action, &r1), iter_key(&action, &r2));
    }

    #[test]
    fn iter_key_blocker_order_independent() {
        let action = make_action("fix", TargetEffect::Advances);
        let mut r1 = make_report(0.5, 1.0, vec![]);
        r1.blockers = Some(vec!["b".to_string(), "a".to_string()]);
        let mut r2 = make_report(0.5, 1.0, vec![]);
        r2.blockers = Some(vec!["a".to_string(), "b".to_string()]);

        assert_eq!(iter_key(&action, &r1), iter_key(&action, &r2));
    }
}
