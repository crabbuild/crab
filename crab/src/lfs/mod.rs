//! Git LFS compatibility layer.
//!
//! Provides Crab-managed Git LFS support for Crab's serverless architecture.
//! LFS objects are stored directly in cloud object storage alongside xorbs,
//! with Crab acting as a standalone transfer agent rather than relying on
//! an HTTP Batch API server.
//!
//! # Serverless Architecture
//!
//! Crab has no servers — all LFS features work through the CLI ↔ git
//! integration and the standalone transfer agent protocol over stdin/stdout:
//!
//! - **Transfer agent** (`transfer_agent`): Communicates with git via
//!   JSON-line protocol (init/upload/download/terminate), supporting
//!   concurrent transfers, progress reporting, and streaming
//!   multipart uploads for objects > 64 MB.
//! - **Filter process** (`filter_process`): Handles clean/smudge via
//!   the long-running git filter protocol v2, dispatching to LFS or
//!   XET handlers based on `.gitattributes` rules.
//! - **Object store** (`crab-lfs`): Stores LFS objects in S3/GCS/Azure
//!   with SHA-256 integrity verification, idempotent puts, and
//!   two-level fan-out layout.
//! - **Locks** (`lock`): Advisory file locking for Crab's CLI and pre-push
//!   checks, backed by CAS in object storage; no HTTP locking endpoint is
//!   exposed.
//! - **Lifecycle** (`lifecycle`): Legacy lifecycle helpers; canonical CLI
//!   prune/fsck paths own user-facing maintenance behavior.
//!
//! # LFS/XET Interoperability
//!
//! The [`crate::git::filter_attr_cache::FilterAttrCache`] resolves
//! `.gitattributes` filter rules with git's "last match wins" semantics.
//! When both `filter=lfs` and `filter=crab` match the same file, the
//! later line wins. Explicit attributes are the canonical routing contract.

pub mod batch;
pub(crate) mod cache;
pub mod config;
pub(crate) mod coordinator;
pub mod extension;
pub mod fetch_filter;
pub mod lifecycle;
pub mod lock;
pub mod migrate;
pub mod prune;
pub(crate) mod publication;
pub(crate) mod recent;
pub mod status;
pub mod track;
pub mod transfer_agent;
