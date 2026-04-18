//! Wire protocol types.
//!
//! These model the JSON contract between converge and fitness skills.
//! Fitness skills emit `FitnessReport` on stdout (exit 0). Converge
//! reads it, makes loop decisions, and never interprets domain fields.

use serde::{Deserialize, Serialize};

/// Fitness skill observation. The only fields converge interprets are
/// `score`, `target`, `actions`, `terminal`, `blockers`, and
/// `blocker_split.structural`. Everything else is passed through to
/// hooks and exit.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FitnessReport {
    pub score: f64,
    pub target: f64,
    pub actions: Vec<Action>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_emoji: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axes: Option<Vec<AxisLine>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blockers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker_split: Option<BlockerSplit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_state: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<Terminal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub kind: String,
    pub description: String,
    pub automation: Automation,
    pub target_effect: TargetEffect,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execute: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_poll_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Automation {
    Full,
    Agent,
    Wait,
    Human,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetEffect {
    Advances,
    Blocks,
    Neutral,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerSplit {
    #[serde(default)]
    pub agent: Vec<String>,
    #[serde(default)]
    pub human: Vec<String>,
    #[serde(default)]
    pub structural: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Terminal {
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxisLine {
    pub name: String,
    pub emoji: String,
    pub summary: String,
}
