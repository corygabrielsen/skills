//! Halt status and exit code mapping.

use serde::{Deserialize, Serialize};

use crate::protocol::{Action, Automation, FitnessReport};

/// Terminal outcome of the convergence loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltStatus {
    Success,
    Stalled,
    Timeout,
    Hil,
    AgentNeeded,
    Terminal,
    Error,
    Cancelled,
    FitnessUnavailable,
}

impl HaltStatus {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Stalled => 1,
            Self::Timeout => 2,
            Self::Hil => 3,
            Self::Error => 4,
            Self::AgentNeeded => 5,
            Self::Terminal => 6,
            Self::Cancelled => 7,
            Self::FitnessUnavailable => 8,
        }
    }
}

/// Per-iteration audit record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterLog {
    pub iter: u32,
    pub score: f64,
    pub action_summary: ActionSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionSummary {
    pub kind: String,
    pub automation: Automation,
}

/// Structured cause for error and fitness_unavailable halts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorCause {
    pub source: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_kind: Option<String>,
}

/// The full halt report written to exit.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaltReport {
    pub stage: String,
    pub status: HaltStatus,
    pub timestamp: String,
    pub session_id: String,
    pub resume_cmd: Vec<String>,
    pub iterations: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structural_blockers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<ErrorCause>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<IterLog>,
}

/// Event sent to the hook coprocess via JSONL on stdin.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HookEvent<'a> {
    Iteration {
        iter: u32,
        report: &'a FitnessReport,
        action: &'a Action,
    },
    Halt {
        halt: &'a HaltReport,
        last_report: Option<&'a FitnessReport>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_success() {
        assert_eq!(HaltStatus::Success.exit_code(), 0);
    }

    #[test]
    fn exit_code_stalled() {
        assert_eq!(HaltStatus::Stalled.exit_code(), 1);
    }

    #[test]
    fn exit_code_timeout() {
        assert_eq!(HaltStatus::Timeout.exit_code(), 2);
    }

    #[test]
    fn exit_code_hil() {
        assert_eq!(HaltStatus::Hil.exit_code(), 3);
    }

    #[test]
    fn exit_code_error() {
        assert_eq!(HaltStatus::Error.exit_code(), 4);
    }

    #[test]
    fn exit_code_agent_needed() {
        assert_eq!(HaltStatus::AgentNeeded.exit_code(), 5);
    }

    #[test]
    fn exit_code_terminal() {
        assert_eq!(HaltStatus::Terminal.exit_code(), 6);
    }

    #[test]
    fn exit_code_cancelled() {
        assert_eq!(HaltStatus::Cancelled.exit_code(), 7);
    }

    #[test]
    fn exit_code_fitness_unavailable() {
        assert_eq!(HaltStatus::FitnessUnavailable.exit_code(), 8);
    }

    #[test]
    fn halt_report_serialization() {
        let report = HaltReport {
            stage: "final".to_string(),
            status: HaltStatus::Success,
            timestamp: "2026-04-17T12:00:00Z".to_string(),
            session_id: "test-session".to_string(),
            resume_cmd: vec!["converge".to_string(), "--".to_string(), "fitness".to_string()],
            iterations: 3,
            final_score: Some(1.0),
            structural_blockers: None,
            action: None,
            terminal: None,
            cause: None,
            history: vec![],
        };

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["stage"], "final");
        assert_eq!(json["status"], "success");
        assert_eq!(json["timestamp"], "2026-04-17T12:00:00Z");
        assert_eq!(json["session_id"], "test-session");
        assert_eq!(json["iterations"], 3);
        assert_eq!(json["final_score"], 1.0);
        // Optional None fields should be absent.
        assert!(json.get("structural_blockers").is_none());
        assert!(json.get("action").is_none());
        assert!(json.get("terminal").is_none());
        assert!(json.get("cause").is_none());
        // Empty history should be absent (skip_serializing_if = "Vec::is_empty").
        assert!(json.get("history").is_none());
    }

    #[test]
    fn halt_report_roundtrip_with_cause() {
        let report = HaltReport {
            stage: "final".to_string(),
            status: HaltStatus::Error,
            timestamp: "2026-04-17T12:00:00Z".to_string(),
            session_id: "err-session".to_string(),
            resume_cmd: vec![],
            iterations: 1,
            final_score: None,
            structural_blockers: None,
            action: None,
            terminal: None,
            cause: Some(ErrorCause {
                source: "fitness".to_string(),
                message: "command not found".to_string(),
                stderr: Some("sh: fitness: not found".to_string()),
                action_kind: None,
            }),
            history: vec![IterLog {
                iter: 1,
                score: 0.5,
                action_summary: ActionSummary {
                    kind: "rebase".to_string(),
                    automation: Automation::Full,
                },
            }],
        };

        let json_str = serde_json::to_string(&report).unwrap();
        let roundtripped: HaltReport = serde_json::from_str(&json_str).unwrap();
        assert_eq!(roundtripped.status, HaltStatus::Error);
        assert_eq!(roundtripped.cause.as_ref().unwrap().source, "fitness");
        assert_eq!(roundtripped.history.len(), 1);
        assert_eq!(roundtripped.history[0].iter, 1);
    }
}
