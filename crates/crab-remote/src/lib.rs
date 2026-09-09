//! Shared remote operation orchestration; callers retain authorization and policy.

#[cfg(feature = "local")]
pub mod config;
#[cfg(feature = "local")]
pub mod local;
#[cfg(feature = "publication")]
pub mod publication;
#[cfg(feature = "local")]
pub mod transfer;

#[cfg(feature = "publication")]
pub mod prepare;
#[cfg(feature = "publication")]
pub mod protected;

#[cfg(feature = "publication")]
pub mod objects;
