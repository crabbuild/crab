//! Bounded pointer inspection through the canonical reachable-object walker.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::Path;

use gix_object::{Find, FindHeader};

use super::{PointerBlob, Result, WalkError, walk_ref_union};
use crate::batch::BlobHeader;

/// Pointer candidates and the remaining blob bodies needed for a complete scan.
#[derive(Debug)]
pub struct PointerScan {
    pub pointers: Vec<PointerBlob>,
    /// Must be byte-verified before candidates may become an integrity proof.
    pub unchecked_blobs: Vec<BlobHeader>,
}

/// Resource ceilings for one complete pointer scan, shared across all refs.
#[derive(Debug, Clone, Copy)]
pub struct PointerScanLimits {
    /// Maximum distinct reachable objects across all ref closures.
    pub objects: usize,
    /// Maximum header/body lookups, including repeated shared history.
    pub lookups: usize,
    /// Gitoxide's maximum single allocation for untrusted loose/packed data.
    pub allocation_bytes: usize,
}

/// Scan exact ref targets without retaining one full closure per ref.
///
/// Annotated tags are resolved from object bytes, not mutable refs or peel hints.
/// Cancellation is cooperative between object reads; no worker is detached.
/// Decoded objects are verified; large-blob headers require the returned body's
/// byte verification before these candidates may become an integrity proof.
pub fn scan_pointers(
    git_dir: &Path,
    refs: &[(String, String)],
    limits: PointerScanLimits,
    cancelled: &dyn Fn() -> bool,
) -> Result<PointerScan> {
    if cancelled() {
        return Err(WalkError::Cancelled);
    }
    let objects_dir = git_dir.join("objects");
    let inner = gix_odb::at_opts(
        &objects_dir,
        [],
        gix_odb::store::init::Options {
            alloc_limit_bytes: Some(limits.allocation_bytes),
            ..Default::default()
        },
    )
    .map_err(|source| WalkError::Git {
        operation: format!("failed to open Git ODB at {}", objects_dir.display()),
        source: Box::new(source),
    })?;
    let odb = CheckedOdb {
        inner,
        remaining: Cell::new(limits.lookups),
        maximum_objects: limits.objects,
        failure: Cell::new(None),
        cancelled,
        unchecked_blobs: RefCell::new(BTreeMap::new()),
    };
    let result = walk_ref_union(&odb, refs, limits.objects);
    // Git traversal wraps Find errors (including a cancelled commit lookup).
    // Keep the first budget failure authoritative instead of misreporting it
    // as missing history or allowing a partial scan to become a proof.
    match odb.failure.get() {
        Some(ScanFailure::Cancelled) => return Err(WalkError::Cancelled),
        Some(ScanFailure::Lookups) => {
            return Err(WalkError::LookupLimitExceeded {
                maximum: limits.lookups,
            });
        }
        Some(ScanFailure::Objects(actual)) => {
            return Err(WalkError::LimitExceeded {
                actual,
                maximum: limits.objects,
            });
        }
        None => {}
    }
    let reachable = result?;
    if cancelled() {
        return Err(WalkError::Cancelled);
    }
    Ok(PointerScan {
        pointers: reachable
            .pointers
            .into_iter()
            .map(|pointer| (pointer.oid, pointer))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect(),
        unchecked_blobs: odb
            .unchecked_blobs
            .into_inner()
            .into_iter()
            .map(|(oid, size)| BlobHeader { oid, size })
            .collect(),
    })
}

#[derive(Clone, Copy)]
enum ScanFailure {
    Cancelled,
    Lookups,
    Objects(usize),
}

struct CheckedOdb<'a, T> {
    inner: T,
    remaining: Cell<usize>,
    maximum_objects: usize,
    failure: Cell<Option<ScanFailure>>,
    cancelled: &'a dyn Fn() -> bool,
    unchecked_blobs: RefCell<BTreeMap<[u8; 20], u64>>,
}

impl<T> CheckedOdb<'_, T> {
    fn admit(&self) -> std::result::Result<(), gix_object::find::Error> {
        let failure = self.failure.get().or_else(|| {
            if (self.cancelled)() {
                Some(ScanFailure::Cancelled)
            } else if self.remaining.get() == 0 {
                Some(ScanFailure::Lookups)
            } else {
                None
            }
        });
        if let Some(failure) = failure {
            self.failure.set(Some(failure));
            return Err(Box::new(std::io::Error::other("Git pointer scan stopped")));
        }
        self.remaining.set(self.remaining.get() - 1);
        Ok(())
    }
}

impl<T: Find> Find for CheckedOdb<'_, T> {
    fn try_find<'a>(
        &self,
        id: &gix_hash::oid,
        buffer: &'a mut Vec<u8>,
    ) -> std::result::Result<Option<gix_object::Data<'a>>, gix_object::find::Error> {
        self.admit()?;
        let data = self.inner.try_find(id, buffer)?;
        if let Some(data) = &data {
            // The ODB resolves paths/pack offsets, but does not promise to hash
            // every returned object. Exact-OID proof also needs that binding.
            data.verify_checksum(id)?;
        }
        Ok(data)
    }
}

impl<T: FindHeader> FindHeader for CheckedOdb<'_, T> {
    fn try_header(
        &self,
        id: &gix_hash::oid,
    ) -> std::result::Result<Option<gix_object::Header>, gix_object::find::Error> {
        self.admit()?;
        let header = self.inner.try_header(id)?;
        if let Some(header) = &header
            && header.kind == gix_object::Kind::Blob
            && header.size > crab_types::pointer::MAX_POINTER_SIZE as u64
        {
            let old = self
                .unchecked_blobs
                .borrow_mut()
                .insert(super::oid_to_bytes(id), header.size);
            if old.is_some_and(|size| size != header.size) {
                return Err(Box::new(std::io::Error::other(
                    "Git blob header changed during scan",
                )));
            }
            let count = self.unchecked_blobs.borrow().len();
            if count > self.maximum_objects {
                self.failure.set(Some(ScanFailure::Objects(count)));
                return Err(Box::new(std::io::Error::other(
                    "Git blob verification inventory exceeds object limit",
                )));
            }
        }
        Ok(header)
    }
}

#[cfg(test)]
mod tests;
