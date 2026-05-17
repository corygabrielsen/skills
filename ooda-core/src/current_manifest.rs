//! Single mutable pointer to the current per-iteration state of a PR.
//!
//! `CurrentManifest` is written atomically to `<pr_root>/CURRENT.json`
//! after each iteration. It carries the run-id, iteration number,
//! exit code, a short headline, and a map of symbolic artifact names
//! to relative paths inside the per-iteration immutable directory
//! (`runs/<run-id>/iterations/<NNNN>/`). The actual artifact bytes
//! never move and never change once written; only this manifest
//! mutates, and only by atomic replace.
//!
//! Agents read this file to discover the current state, then follow
//! `artifacts.*` paths to read the underlying immutable records.
//! Historical iterations remain addressable by their per-iteration
//! path; CURRENT.json only names the current one.
//!
//! `keep_runs` lists run-ids that future garbage collection must
//! preserve. The current run is implicitly retained (its run-id is
//! `run_id`). Callers append additional run-ids here when they want
//! to pin older runs for inspection.
//!
//! Blob-hash references are intentionally absent from v1: every
//! per-iteration artifact and the run-level outcome are already
//! content-addressed in `blobs/sha256/` (the recorder copies them
//! there at write time). Readers wanting integrity verification or
//! blob-store retrieval re-hash the file content or walk
//! `blobs/sha256/` directly. If a future version surfaces hashes in
//! the manifest, bump [`SCHEMA_VERSION`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Current schema version. Bump on any incompatible change to the
/// shape of [`CurrentManifest`]; readers must check `schema_version`
/// before consuming.
pub const SCHEMA_VERSION: u32 = 1;

/// The mutable pointer at `<pr_root>/CURRENT.json`. See module docs.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct CurrentManifest {
    pub schema_version: u32,
    pub run_id: String,
    pub iteration: u32,
    pub exit_code: u8,
    pub headline: String,
    /// Symbol → relative path under the `pr_root`. Always points into
    /// `runs/<run-id>/iterations/<NNNN>/` (or `runs/<run-id>/` for
    /// `outcome`). Conditional artifacts (`handoff`, `action`) are
    /// simply absent when the iteration did not produce them.
    pub artifacts: BTreeMap<String, PathBuf>,
    /// Run-ids that garbage collection must preserve in addition to
    /// `run_id` itself. Empty by default.
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
