//! Direct storage-provider and content-cache configuration.

#[cfg(feature = "files")]
pub use crate::content::ContentCache;
pub use crate::store_options::{AzureOptions, DirectStoreOptions, GcsOptions, S3Options};
