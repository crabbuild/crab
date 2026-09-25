//! Unstable inspection surface for external fuzzers, auditors, and tools.
//!
//! Nothing here carries a compatibility guarantee: it exists so a verifier can
//! exercise the same decoders production uses without depending on private
//! modules. Production callers use the typed APIs instead. The shapes follow
//! Celld's `internal` module for the same purpose.

use crate::Result;

/// Summary of one decoded LTX stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectedLtx {
    /// Page size the file was encoded with.
    pub page_size: u32,
    /// Database page count the commit published.
    pub commit: u32,
    /// First transaction id the file carries.
    pub min_txid: u64,
    /// Last transaction id the file carries.
    pub max_txid: u64,
    /// Rolling database checksum the file expects before it applies.
    pub pre_apply_checksum: u64,
    /// Rolling database checksum the file publishes after it applies.
    pub post_apply_checksum: u64,
    /// Number of page frames the file carries.
    pub pages: u32,
    /// Exact encoded length in bytes.
    pub size_bytes: u64,
    /// BLAKE3 digest of the encoded bytes.
    pub blake3: [u8; 32],
}

/// Decodes one complete LTX stream and reports what it carries.
///
/// Every structural rule the production decoder enforces applies: checksum
/// trailer, page order, page coverage, index agreement, and the rolling
/// database checksum for snapshots.
pub fn inspect_ltx(bytes: &[u8]) -> Result<InspectedLtx> {
    crate::ltx::inspect_bytes(bytes)
}

/// Decodes and shape-checks one Cell root document.
#[cfg(feature = "replica")]
pub fn inspect_root(bytes: &[u8]) -> Result<usize> {
    crate::replica::root::inspect_root(bytes)
}

/// Decodes one root descriptor page and reports its descriptor count.
#[cfg(feature = "replica")]
pub fn inspect_segment_page(bytes: &[u8]) -> Result<usize> {
    crate::replica::root::inspect_segment_page(bytes)
}

/// Shape-checks one encoded directory node.
///
/// See [`crate::replica::directory::inspect_node`]: extent membership and the
/// lock-page rule need a root graph and are not checked here.
#[cfg(feature = "replica")]
pub fn inspect_directory_node(bytes: &[u8]) -> Result<()> {
    crate::replica::directory::inspect_node(bytes)
}

/// Decodes one checked Cell bundle and reports its row count.
#[cfg(feature = "replica")]
pub fn inspect_bundle(bytes: &[u8], limits: crate::Limits) -> Result<usize> {
    let bundle = crate::bundle::Bundle::decode(bytes.to_vec(), limits)?;
    Ok(bundle.rows().len())
}

/// Decodes one authenticated node frame and reports its body length.
#[cfg(feature = "replica")]
pub fn inspect_node_frame(bytes: &[u8], limits: crate::Limits) -> Result<usize> {
    let frame = crate::inspect_node_frame(bytes.to_vec().into(), limits)?;
    Ok(frame.body().len())
}

/// Reports the LTX file-size bound for a page count, for external admission tools.
pub fn cut_upper_bound(page_size: u32, pages: u64) -> Result<u64> {
    crate::ltx::cut_upper_bound(page_size, pages)
}

/// Reports the LTX frame bound for one page, for external admission tools.
pub fn frame_upper_bound(page_size: u32) -> Result<u64> {
    crate::ltx::frame_payload_upper_bound(page_size)
}
