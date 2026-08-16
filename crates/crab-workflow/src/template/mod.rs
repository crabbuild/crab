//! Template resolution layer for `crab.yaml`.
//!
//! Resolves `${...}` expressions against a merged context built from
//! inline `vars:`, params files, and (optionally) environment
//! variables. All resolution happens at parse time — the downstream
//! scheduler and executor never see template syntax.

pub mod context;
pub mod foreach;
pub mod matrix;
pub mod substitute;

pub use context::TemplateContext;
pub use foreach::expand_foreach;
pub use matrix::expand_matrix;
pub use substitute::{substitute, substitute_cmd};
