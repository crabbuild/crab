//! Git LFS compatibility layer.
//!
//! Provides full Git LFS support for Crab's serverless architecture.
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
//! - **Locks** (`lock`): Advisory file locking compatible with the
//!   Git LFS File Locking API, backed by CAS in object storage.
//! - **Lifecycle** (`lifecycle`): Prune unreferenced objects, verify
//!   integrity with fsck, and generate cloud lifecycle policies.
//!
//! # LFS/XET Interoperability
//!
//! The [`crate::git::filter_attr_cache::FilterAttrCache`] resolves
//! `.gitattributes` filter rules with git's "last match wins" semantics.
//! When both `filter=lfs` and `filter=crab` match the same file, the
//! later line wins. Users can override automatic routing via explicit
//! `.gitattributes` entries.
//!
//! The [`crate::routing::engine`] provides an intelligent routing
//! decision engine that chooses LFS vs XET based on file size, version
//! count, and content entropy.

pub mod batch;
pub mod config;
pub mod extension;
pub mod fetch_filter;
pub mod lifecycle;
pub mod lock;
pub mod migrate;
pub mod prune;
pub(crate) mod recent;
pub mod status;
pub mod track;
pub mod transfer_agent;
