//! Shared fixtures and recovery checks; test cases stay with their owners.

pub(crate) mod archive;
pub(crate) mod git;
pub(crate) mod http;
pub(crate) mod server;

#[cfg(unix)]
pub(crate) mod tls;
