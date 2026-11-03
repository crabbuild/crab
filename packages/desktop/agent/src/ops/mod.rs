//! Resumable operation state for merge / rebase / cherry-pick.
//!
//! Each in-flight operation is tracked as an [`OpRecord`] persisted to
//! `.crab/desktop/ops/<id>.json`. The UI can resume from any view
//! change or crash by loading the record and continuing from the
//! stored state.

mod driver;
pub mod handlers;

pub use driver::{
    MergeStrategy, Op, OpRecord, OpState, TodoAction, TodoItem, load_all, ops_dir_for_repo,
};
