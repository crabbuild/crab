#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::await_holding_lock
)]
#![warn(clippy::perf, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::module_name_repetitions,
    clippy::struct_excessive_bools,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::result_large_err
)]

//! Crab — serverless git remote helper and large-file filter driver.
//!
//! See `AGENTS.md` for the project overview and coding conventions.

pub mod audit;
pub mod auth;
pub mod cache;
pub mod cmd;
pub mod coordination;
pub mod core;
pub mod cost;
pub mod diff;
pub mod engine;
pub mod git;
pub mod hydrate;
pub mod import;
pub mod lfs;
pub(crate) mod maintenance;
pub mod metadata;
pub mod optimize;
pub mod read;
pub mod release;
pub mod replication;
pub mod storage;
pub mod tier;
pub use crab_vfs as vfs;
pub use crab_workflow as workflow;

pub mod speculation;

#[cfg(test)]
pub mod test;
