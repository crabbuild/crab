//! Direct storage-provider and content-cache configuration.

#[cfg(feature = "content")]
pub use crate::content::ContentCache;
pub use crate::store_options::{AzureOptions, DirectStoreOptions, GcsOptions, S3Options};
