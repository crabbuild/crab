//! Cost optimizer subsystem — inventory collection, pricing model,
//! recommendations engine, and report rendering for `crab doctor --cost`.
//!
//! Consumes object inventories (live list or provider-side reports),
//! applies a versioned pricing model with optional user overrides, and
//! produces actionable cost-reduction recommendations.
//!
//! # Submodules
//!
//! - `pricing/` — `PriceTable`, embedded data (build-time generated),
//!   override file loading and merge.
//! - `inventory/` — `Inventory` type, live walker, provider-side report
//!   parsers (S3/GCS/Azure), deterministic sampling.
//! - `recommendations` — rule engine and built-in rules.
//! - `report` — human (`comfy-table`) and JSON formatters.

pub mod inventory;
pub mod pricing;
pub mod recommendations;
pub mod report;
