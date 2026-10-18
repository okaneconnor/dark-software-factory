//! The side-effecting half of dsf: host git, containers, GitHub, storage
//! and the board. The rules that decide what happens live in `dsf-core`.

pub mod agent;
pub mod engine;
pub mod factory;
pub mod github;
pub mod hostgit;
pub mod masker;
pub mod prompts;
pub mod proxy;
pub mod rundir;
pub mod sandbox;
pub mod store;
