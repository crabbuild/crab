//! Cell lifecycle scheduling for one Cell actor.
//!
//! Activation, bootstrap, background hydration/inventory/compaction, eviction,
//! renewals, transfer inspection, and deactivation all schedule the next step
//! the loop should take.

use super::admission::{fence_active, finish_migration, send_migration_reply};
use super::*;

mod activation;
mod background;
mod eviction;
mod scheduling;

pub(super) use activation::*;
pub(super) use background::*;
pub(super) use eviction::*;
pub(super) use scheduling::*;
