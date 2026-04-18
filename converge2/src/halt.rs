//! Halt status and exit code mapping.

use serde::{Deserialize, Serialize};

use crate::protocol::{Action, FitnessReport};

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
    pub automation: String,
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
