//! Mutable pointer over an immutable per-iteration history.
//!
//! The recorder layout is a write-once history (one immutable
//! directory per iteration, with all artifact bytes content-stable)
//! plus a single mutable pointer file that names the current
//! iteration. `CurrentManifest` is that pointer.
//!
//! # Invariants
//!
//! - **Immutability of history**: per-iteration artifact bytes are
//!   never rewritten after their initial write.
//! - **Single mutable head**: only this manifest mutates between
//!   iterations, and only by atomic replace
//!   (see [`crate::atomic_io`]) — concurrent readers observe either
//!   the prior or the new manifest, never a torn intermediate.
//! - **Address stability**: every historical iteration remains
//!   addressable by its own path; the manifest names only the head.
//!
//! Readers resolve `artifacts.*` relative paths against the
//! recorder root to read the underlying immutable records.
//! `keep_runs` pins additional run identifiers for retention
//! against garbage collection; the current run is implicitly pinned.
//!
//! Integrity verification and content-addressed retrieval are out
//! of scope for v1: per-iteration artifacts are already hashed at
//! write time by the recorder, and readers re-hash the file content
//! directly. Surfacing hashes in the manifest itself would be a
//! breaking change — bump [`SCHEMA_VERSION`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Current schema version. Bump on any incompatible change to the
/// shape of [`CurrentManifest`]; readers must check `schema_version`
/// before consuming.
pub const SCHEMA_VERSION: u32 = 1;

/// Mutable head pointer over the immutable iteration history.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CurrentManifest {
    pub schema_version: u32,
    pub run_id: String,
    pub iteration: u32,
    pub exit_code: u8,
    pub headline: String,
    /// Symbolic-name → relative path within the recorder root.
    /// Paths address artifacts in the current iteration's immutable
    /// directory (or the run root for run-scoped artifacts).
    /// Conditional artifacts are simply absent when the iteration
    /// did not produce them.
    pub artifacts: BTreeMap<String, PathBuf>,
    /// Additional run identifiers pinned against garbage collection.
    /// The current run (`run_id`) is implicitly pinned and need not
    /// appear here.
    pub keep_runs: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CurrentManifest {
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "handoff".to_string(),
            PathBuf::from("runs/r1/iterations/0005/handoff.md"),
        );
        artifacts.insert(
            "state".to_string(),
            PathBuf::from("runs/r1/iterations/0005/oriented.json"),
        );
        CurrentManifest {
            schema_version: SCHEMA_VERSION,
            run_id: "r1".to_string(),
            iteration: 5,
            exit_code: 4,
            headline: "HandoffAgent: AddressThreads".to_string(),
            artifacts,
            keep_runs: vec![],
        }
    }

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, 1);
    }

    #[test]
    fn round_trip_serde() {
        let manifest = sample();
        let json = serde_json::to_string(&manifest).expect("serialize");
        let back: CurrentManifest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, manifest);
    }

    #[test]
    fn json_shape_golden_v1() {
        let manifest = sample();
        let json = serde_json::to_value(&manifest).expect("serialize");
        let expected = serde_json::json!({
            "schema_version": 1,
            "run_id": "r1",
            "iteration": 5,
            "exit_code": 4,
            "headline": "HandoffAgent: AddressThreads",
            "artifacts": {
                "handoff": "runs/r1/iterations/0005/handoff.md",
                "state": "runs/r1/iterations/0005/oriented.json",
            },
            "keep_runs": [],
        });
        assert_eq!(json, expected);
    }

    #[test]
    fn artifacts_btreemap_serializes_deterministically() {
        // BTreeMap iteration order is sorted; two manifests built in
        // different insertion orders must produce identical JSON.
        let mut a_artifacts = BTreeMap::new();
        a_artifacts.insert("z".to_string(), PathBuf::from("z"));
        a_artifacts.insert("a".to_string(), PathBuf::from("a"));
        let mut b_artifacts = BTreeMap::new();
        b_artifacts.insert("a".to_string(), PathBuf::from("a"));
        b_artifacts.insert("z".to_string(), PathBuf::from("z"));
        let make = |artifacts| CurrentManifest {
            schema_version: SCHEMA_VERSION,
            run_id: "r".into(),
            iteration: 0,
            exit_code: 0,
            headline: String::new(),
            artifacts,
            keep_runs: vec![],
        };
        assert_eq!(
            serde_json::to_string(&make(a_artifacts)).unwrap(),
            serde_json::to_string(&make(b_artifacts)).unwrap(),
        );
    }
}
