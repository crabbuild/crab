//! Standard Git LFS HTTP gateway for Crab.
//!
//! The gateway owns HTTP discovery, Batch/basic transfer negotiation, file
//! locking, authentication, and repository policy. Verified object bytes and
//! the shared lock record format remain owned by `crab-lfs`.

pub mod auth;
pub mod config;
pub mod error;
pub mod http;
pub mod metrics;
pub mod server;

pub use auth::{AuthConfig, AuthPolicy, ClientIdentity, PolicyRule, TlsClientIdentity};
pub use config::{ActionSecret, LfsServerConfig, TlsConfig};
pub use error::{LfsServerError, Result};
pub use metrics::LfsMetrics;
pub use server::{PreparedServer, ServerStartupOptions, prepare_server, run_server};
