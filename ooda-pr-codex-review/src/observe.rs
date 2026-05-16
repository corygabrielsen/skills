//! Observe stage: gather raw signals about the PR from external
//! sources. No interpretation — downstream stages decide what the
//! signals mean.

pub(crate) mod codex;
pub(crate) mod github;
