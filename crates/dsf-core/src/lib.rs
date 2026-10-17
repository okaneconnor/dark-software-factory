//! The pure domain model behind `dsf`: configuration, workflow definitions,
//! stage routing, the GitHub label lifecycle and scheduling rules.
//!
//! This crate performs no IO — no filesystem, network, process or clock
//! access. Everything here is deterministic and unit-testable; the `dsf`
//! binary crate owns every side effect.
