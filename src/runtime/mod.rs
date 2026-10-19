//! Capability runtime orchestration (design-capability-split §4, M1).
//!
//! main() is being decomposed into per-capability orchestration modules
//! under this tree; the thin bin shells (§2) will assemble them once the
//! extraction is complete. Modules land one slice per commit, each with
//! zero behavior change against the previous layout.

pub mod bootstrap;
