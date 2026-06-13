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

/// 24-hour ceiling on a wait interval. Anything beyond this is operator
/// error from a fitness skill; the upper bound also keeps the resulting
/// `Duration` well below the saturating `u64::MAX` value that would panic
/// `Instant + Duration` on overflow inside `interruptible_sleep`.
const MAX_POLL_MS: u64 = 86_400_000;

/// Fallback wait when `next_poll_seconds` is absent or non-finite.
const DEFAULT_POLL_MS: u64 = 60_000;

pub(crate) struct ConvergeOpts {
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

/// Convert a fitness-skill `next_poll_seconds` to a bounded millisecond count.
///
/// Inputs originate in untrusted JSON. `serde_json` parses `1e400` as
/// `f64::INFINITY` and may also produce `NaN`; both would propagate into
/// `Instant + Duration::from_millis(u64::MAX)` and panic the binary.
///
/// Invariants:
/// - Output is always in `[0, MAX_POLL_MS]`.
/// - `None`, `NaN`, or any non-finite input yields `DEFAULT_POLL_MS`.
/// - Negative input is floored to `0`.
/// - Finite positive input above the 24h ceiling clamps to `MAX_POLL_MS`.
fn clamp_poll_ms(secs: Option<f64>) -> u64 {
    let Some(secs) = secs else {
        return DEFAULT_POLL_MS;
    };
    #[allow(clippy::cast_precision_loss)]
    let max_ms_f = MAX_POLL_MS as f64;
    let ms_f = (secs * 1000.0).clamp(0.0, max_ms_f);
    if ms_f.is_finite() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ms = ms_f as u64;
        ms
    } else {
        DEFAULT_POLL_MS
    }
}

fn action_summary(action: &Action) -> ActionSummary {
    ActionSummary {
        kind: action.kind.clone(),
        automation: action.automation,
    }
}

/// Canonical, injective stringification for iteration-key dedup.
///
/// Equal-logical inputs produce equal output; distinct-logical inputs produce
/// distinct output. Object keys are sorted lexicographically; strings (and
/// object keys) are escaped by `serde_json`, which handles `\`, `"`, control
/// chars, and unicode correctly.
fn stable_json(value: &serde_json::Value) -> String {
    serde_json::to_string(&canonicalize(value)).expect("serde_json::Value always serializes")
}

/// Rebuild a `Value` so every nested object has keys in lexicographic order.
/// Array element order is preserved; atoms are returned as-is.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<&String, &serde_json::Value> = map.iter().collect();
            let mut out = serde_json::Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k.clone(), canonicalize(v));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(canonicalize).collect())
        }
        atom => atom.clone(),
    }
}

/// Iteration key: identity of a logical state. Same key = same iteration.
fn iter_key(action: &Action, report: &FitnessReport) -> String {
    let mut blockers: Vec<String> = report.blockers.as_deref().unwrap_or_default().to_vec();
    blockers.sort();
    let blocker_str = blockers.join("|");

    let activity = report.activity_state.as_ref().map_or_else(
        || "{}".to_string(),
        |m| stable_json(&serde_json::Value::Object(m.clone())),
    );

    let type_digest = action
        .r#type
        .as_ref()
        .map_or_else(|| "null".to_string(), stable_json);

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

// Flat observe→decide→act loop with one-shot poll, full execution, and
// halt branches. Length IS the spec; each block names its phase inline
// (observe, classify, dispatch, repeat). Splitting into helpers would
// shred the loop invariants across files.
#[allow(clippy::too_many_lines)]
pub(crate) fn converge(opts: &ConvergeOpts, cancelled: &AtomicBool) -> Result<HaltReport, String> {
    let mut session = Session::open(&opts.session_id)?;

    let mut hook = opts
        .hook_cmd
        .as_deref()
        .map(Hook::spawn)
        .transpose()
        .map_err(|e| format!("cannot spawn hook: {e}"))?;

    // Iteration history length fits in u32: max_iter is u32-bounded
    // and the loop terminates well before usize overflow could occur.
    let start_iter = u32::try_from(session.history.len()).expect("history length fits in u32") + 1;
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
            .map_or_else(|| "?".to_string(), |s| s.to_string());
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
            let halt = make_halt(HaltStatus::Cancelled, &session, opts, iter, last_score);
            return finalize(halt, &session, &mut hook, &last_report);
        }
        poll_count += 1;

        trace(opts.verbose, &format!("poll {poll_count} (iter {iter})"));

        // Observe.
        let report = match fitness::invoke(&opts.fitness_argv, cancelled) {
            Ok(r) => r,
            Err(fitness::FitnessError::Cancelled) => {
                let halt = make_halt(HaltStatus::Cancelled, &session, opts, iter, last_score);
                return finalize(halt, &session, &mut hook, &last_report);
            }
            Err(fitness::FitnessError::Permanent(msg)) => {
                let mut halt =
                    make_halt(HaltStatus::FitnessUnavailable, &session, opts, iter, None);
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
                    make_halt(HaltStatus::FitnessUnavailable, &session, opts, iter, None);
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
                opts,
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
                opts,
                iter,
                Some(report.score),
            );
            halt.terminal = report
                .terminal
                .as_ref()
                .map(|t| serde_json::to_value(t).unwrap_or_default());
            return finalize(halt, &session, &mut hook, &last_report);
        }

        let Some(action) = action else {
            let halt = make_halt(
                HaltStatus::Stalled,
                &session,
                opts,
                iter,
                Some(report.score),
            );
            return finalize(halt, &session, &mut hook, &last_report);
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
                    opts,
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
                        make_halt(HaltStatus::Error, &session, opts, iter, Some(report.score));
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
                            make_halt(HaltStatus::Error, &session, opts, iter, Some(report.score));
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
                            make_halt(HaltStatus::Error, &session, opts, iter, Some(report.score));
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
                    opts,
                    iter,
                    Some(report.score),
                );
                halt.action = Some(action.clone());
                return finalize(halt, &session, &mut hook, &last_report);
            }
            Automation::Wait => {
                let ms = clamp_poll_ms(action.next_poll_seconds);
                interruptible_sleep(ms, cancelled)?;
            }
            Automation::Human => {
                let mut halt = make_halt(HaltStatus::Hil, &session, opts, iter, Some(report.score));
                halt.action = Some(action.clone());
                return finalize(halt, &session, &mut hook, &last_report);
            }
        }
    }

    // Poll cap exhausted.
    let halt = make_halt(HaltStatus::Timeout, &session, opts, iter, last_score);
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

    #[test]
    fn stable_json_object_key_order_does_not_affect_output() {
        let a = json!({"a": 1, "b": 2});
        let b = json!({"b": 2, "a": 1});
        assert_eq!(stable_json(&a), stable_json(&b));
    }

    #[test]
    fn stable_json_escapes_backslash_in_string() {
        // Previous implementation left `\` unescaped in string content. The
        // canonical encoder must escape it so a string containing a literal
        // backslash is distinguishable from any other input that would have
        // produced the same output bytes via the old format.
        let a = json!("a\\b"); // 3 chars: a, \, b
        let b = json!("ab"); // 2 chars: a, b
        assert_ne!(stable_json(&a), stable_json(&b));
        assert_eq!(stable_json(&a), r#""a\\b""#);
    }

    #[test]
    fn stable_json_escapes_control_chars_in_string() {
        // Newline and tab in string content must be escaped, otherwise the
        // raw bytes leak into the iter_key separator format and a literal
        // newline collides with the two-char sequence `\n`.
        let nl_literal = json!("\n"); // 1 char: 0x0A
        let nl_escape = json!("\\n"); // 2 chars: \, n
        assert_ne!(stable_json(&nl_literal), stable_json(&nl_escape));

        let tab_literal = json!("\t"); // 1 char: 0x09
        let tab_escape = json!("\\t"); // 2 chars: \, t
        assert_ne!(stable_json(&tab_literal), stable_json(&tab_escape));
    }

    #[test]
    fn stable_json_object_key_with_quote_does_not_collide() {
        // Previous implementation inserted object keys raw with no escaping,
        // so a key containing `","` could mimic the entry separator and
        // shadow a two-entry object.
        let two_entries = json!({"a": null, "b": null});
        let one_entry_injecting_key = serde_json::Value::Object({
            let mut m = serde_json::Map::new();
            m.insert(r#"a":null,"b"#.to_string(), serde_json::Value::Null);
            m
        });
        assert_ne!(
            stable_json(&two_entries),
            stable_json(&one_entry_injecting_key)
        );
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

    // -- clamp_poll_ms --

    #[test]
    fn clamp_poll_ms_reasonable_value_passes_through() {
        assert_eq!(clamp_poll_ms(Some(30.0)), 30_000);
    }

    #[test]
    fn clamp_poll_ms_none_yields_default() {
        assert_eq!(clamp_poll_ms(None), DEFAULT_POLL_MS);
    }

    #[test]
    fn clamp_poll_ms_zero_passes_through() {
        assert_eq!(clamp_poll_ms(Some(0.0)), 0);
    }

    #[test]
    fn clamp_poll_ms_negative_clamps_to_zero() {
        // Untrusted input. A negative value must not roll over to a
        // very large u64 via sign loss; it floors at zero.
        assert_eq!(clamp_poll_ms(Some(-1.0)), 0);
    }

    #[test]
    fn clamp_poll_ms_infinity_clamps_to_ceiling() {
        // `serde_json` parses `1e400` as f64::INFINITY in non-strict mode.
        // Without clamping, `Instant + Duration::from_millis(u64::MAX)`
        // panics the binary instead of producing a clean Outcome.
        assert_eq!(clamp_poll_ms(Some(f64::INFINITY)), MAX_POLL_MS);
    }

    #[test]
    fn clamp_poll_ms_nan_falls_back_to_default() {
        // `f64::NAN * 1000.0` is NaN; `NaN.clamp(0.0, max)` is NaN per
        // stdlib; the helper detects the non-finite result and falls back.
        assert_eq!(clamp_poll_ms(Some(f64::NAN)), DEFAULT_POLL_MS);
    }

    #[test]
    fn clamp_poll_ms_above_ceiling_clamps_to_ceiling() {
        // 48 hours in seconds — well above the 24h ceiling.
        let two_days_secs = 48.0 * 60.0 * 60.0;
        assert_eq!(clamp_poll_ms(Some(two_days_secs)), MAX_POLL_MS);
    }

    #[test]
    fn clamp_poll_ms_at_ceiling_exact() {
        let day_secs = 24.0 * 60.0 * 60.0;
        assert_eq!(clamp_poll_ms(Some(day_secs)), MAX_POLL_MS);
    }

    #[test]
    fn clamp_poll_ms_neg_infinity_clamps_to_zero() {
        assert_eq!(clamp_poll_ms(Some(f64::NEG_INFINITY)), 0);
    }
}
