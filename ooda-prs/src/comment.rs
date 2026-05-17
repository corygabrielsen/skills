//! PR comment surface.
//!
//! Decomposed into rendering (pure projection of orient + decision
//! into the comment body and a stable dedup key) and posting
//! (delivery with content-hash dedup against prior state). Same
//! structural state across iterations collapses to the same key
//! and skips re-posting.

pub(crate) mod post;
pub(crate) mod render;
