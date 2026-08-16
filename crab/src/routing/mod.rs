//! LFS/XET intelligent routing engine.
//!
//! Decides whether a file should be stored via LFS or XET based on:
//! 1. File size threshold (`lfs-xet-threshold`, default 10 MB)
//! 2. Version count (single-version files → LFS)
//! 3. Content entropy (high-entropy/compressed files → LFS)
//! 4. User override via `.gitattributes` (`filter=lfs` or `filter=crab`)
//!
//! The routing decision is made during the filter clean path, after
//! `.gitattributes` dispatch but before pointer creation.

pub mod engine;
