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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_full_fitness_report() {
        let json = r#"{
            "score": 0.75,
            "target": 1.0,
            "actions": [{
                "kind": "rebase",
                "description": "Rebase on latest master",
                "automation": "full",
                "target_effect": "advances",
                "execute": ["git", "rebase", "origin/master"],
                "next_poll_seconds": 30.0,
                "timeout_seconds": 120.0
            }],
            "status": "in_progress",
            "score_display": "75%",
            "target_display": "100%",
            "score_emoji": "🟡",
            "score_label": "Almost there",
            "target_label": "Merge-ready",
            "axes": [{"name": "ci", "emoji": "✅", "summary": "All green"}],
            "snapshot": {"sha": "abc123"},
            "notes": ["CI passed", "Reviews pending"],
            "blockers": ["needs-review"],
            "blocker_split": {
                "agent": ["lint-fix"],
                "human": ["approval"],
                "structural": ["branch-protection"]
            },
            "activity_state": {"last_push": "2026-04-17"},
            "terminal": {"kind": "merged"}
        }"#;

        let report: FitnessReport = serde_json::from_str(json).unwrap();
        assert_eq!(report.score, 0.75);
        assert_eq!(report.target, 1.0);
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].kind, "rebase");
        assert_eq!(report.actions[0].automation, Automation::Full);
        assert_eq!(report.actions[0].target_effect, TargetEffect::Advances);
        assert_eq!(report.actions[0].execute.as_ref().unwrap(), &["git", "rebase", "origin/master"]);
        assert_eq!(report.status.as_deref(), Some("in_progress"));
        assert_eq!(report.score_display.as_deref(), Some("75%"));
        assert_eq!(report.target_display.as_deref(), Some("100%"));
        assert_eq!(report.score_emoji.as_deref(), Some("🟡"));
        assert_eq!(report.score_label.as_deref(), Some("Almost there"));
        assert_eq!(report.target_label.as_deref(), Some("Merge-ready"));
        assert_eq!(report.axes.as_ref().unwrap().len(), 1);
        assert_eq!(report.axes.as_ref().unwrap()[0].name, "ci");
        assert!(report.snapshot.is_some());
        assert_eq!(report.notes.as_ref().unwrap(), &["CI passed", "Reviews pending"]);
        assert_eq!(report.blockers.as_ref().unwrap(), &["needs-review"]);
        let split = report.blocker_split.as_ref().unwrap();
        assert_eq!(split.agent, vec!["lint-fix"]);
        assert_eq!(split.human, vec!["approval"]);
        assert_eq!(split.structural, vec!["branch-protection"]);
        assert!(report.activity_state.is_some());
        assert_eq!(report.terminal.as_ref().unwrap().kind, "merged");
    }

    #[test]
    fn deserialize_minimal_fitness_report() {
        let json = r#"{
            "score": 0.5,
            "target": 1.0,
            "actions": []
        }"#;

        let report: FitnessReport = serde_json::from_str(json).unwrap();
        assert_eq!(report.score, 0.5);
        assert_eq!(report.target, 1.0);
        assert!(report.actions.is_empty());
        assert!(report.status.is_none());
        assert!(report.score_display.is_none());
        assert!(report.target_display.is_none());
        assert!(report.score_emoji.is_none());
        assert!(report.score_label.is_none());
        assert!(report.target_label.is_none());
        assert!(report.axes.is_none());
        assert!(report.snapshot.is_none());
        assert!(report.notes.is_none());
        assert!(report.blockers.is_none());
        assert!(report.blocker_split.is_none());
        assert!(report.activity_state.is_none());
        assert!(report.terminal.is_none());
    }

    #[test]
    fn automation_deserializes_all_variants() {
        assert_eq!(
            serde_json::from_str::<Automation>(r#""full""#).unwrap(),
            Automation::Full
        );
        assert_eq!(
            serde_json::from_str::<Automation>(r#""agent""#).unwrap(),
            Automation::Agent
        );
        assert_eq!(
            serde_json::from_str::<Automation>(r#""wait""#).unwrap(),
            Automation::Wait
        );
        assert_eq!(
            serde_json::from_str::<Automation>(r#""human""#).unwrap(),
            Automation::Human
        );
    }

    #[test]
    fn target_effect_deserializes_all_variants() {
        assert_eq!(
            serde_json::from_str::<TargetEffect>(r#""advances""#).unwrap(),
            TargetEffect::Advances
        );
        assert_eq!(
            serde_json::from_str::<TargetEffect>(r#""blocks""#).unwrap(),
            TargetEffect::Blocks
        );
        assert_eq!(
            serde_json::from_str::<TargetEffect>(r#""neutral""#).unwrap(),
            TargetEffect::Neutral
        );
    }

    #[test]
    fn unknown_automation_value_errors() {
        let result = serde_json::from_str::<Automation>(r#""magic""#);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_target_effect_value_errors() {
        let result = serde_json::from_str::<TargetEffect>(r#""destroys""#);
        assert!(result.is_err());
    }
}
