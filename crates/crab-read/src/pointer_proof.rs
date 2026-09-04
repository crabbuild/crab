//! Bounded origin verification of Crab pointer content from an explicit shard.

use std::{collections::HashSet, time::Duration};

use bytes::Bytes;
use crab_storage::{StorageError, Store, StoreLayout};
use crab_types::pointer::Pointer;
use crab_xet::{
    hash::{MerkleHash, compute_data_hash},
    shard_parse::{
        ExtractedFileRecipe, ExtractedFileTerm, MAX_SHARD_SIZE_BYTES,
        extract_file_recipes_for_hashes_with_limit, visit_file_entries_from_reader,
    },
    xorb::{format::MAX_XORB_SIZE, parser::XorbParser},
};
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, PointerProofError>;

/// Bounds for one origin proof, independent of the pointer's claimed size.
#[derive(Clone, Copy)]
pub struct PointerProofLimits {
    pub max_file_bytes: u64,
    pub max_shard_bytes: u64,
    pub max_xorb_bytes: u64,
    pub max_read_bytes: u64,
    pub max_chunks: usize,
    pub max_duration: Duration,
}

/// A rejected content proof never changes canonical data or creates a receipt.
#[derive(Debug, thiserror::Error)]
pub enum PointerProofError {
    #[error("pointer proof storage read failed")]
    Storage(#[from] StorageError),
    #[error("pointer proof data validation failed")]
    Xet(#[from] crab_xet::error::XetError),
    #[error("pointer proof worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("pointer proof exceeds {0}")]
    Limit(&'static str),
    #[error("invalid pointer content: {0}")]
    Integrity(&'static str),
    #[error("pointer proof cancelled")]
    Cancelled,
    #[error("pointer proof deadline exceeded")]
    Deadline,
}

/// Verifies an explicit shard and all bytes needed by one Crab pointer at origin.
///
/// Returns its exact ordered recipe only after shard, xorb, chunk, size and
/// whole-file hash checks pass. Repeated chunks remain repeated occurrences.
/// One xorb is retained at a time; recurring xorbs count again against the
/// aggregate successful-body budget (transport retries are separately bounded
/// by `Store`). CPU work runs on blocking workers.
/// Cancellation or deadline drops cancel pending workers at chunk boundaries.
///
/// The caller must supply an origin-only store without cache/read routing,
/// prove `shard_hash` belongs to its pinned committed generation,
/// hold GC writer fences through publication, and recheck that base before commit.
/// A supplied pointer hint or this content proof alone cannot establish ownership.
/// No refs, storage receipts or local Git objects are written.
pub async fn verify_crab_pointer(
    layout: &StoreLayout<Store>,
    pointer: &Pointer,
    shard_hash: MerkleHash,
    limits: PointerProofLimits,
    cancel: &CancellationToken,
) -> Result<ExtractedFileRecipe> {
    let cancel = cancel.child_token();
    let _guard = cancel.clone().drop_guard();
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(PointerProofError::Cancelled),
        result = tokio::time::timeout(limits.max_duration, verify_content(layout, pointer, shard_hash, limits, &cancel)) => {
            result.map_err(|_| PointerProofError::Deadline)?
        }
    }
}

struct FileProof {
    recipe: ExtractedFileRecipe,
    cursor: usize,
    hasher: blake3::Hasher,
}

async fn verify_content(
    layout: &StoreLayout<Store>,
    pointer: &Pointer,
    shard_hash: MerkleHash,
    limits: PointerProofLimits,
    cancel: &CancellationToken,
) -> Result<ExtractedFileRecipe> {
    check(cancel)?;
    if pointer.size > limits.max_file_bytes {
        return Err(PointerProofError::Limit("file bytes"));
    }
    let (body, _) = layout
        .store()
        .get_with_etag_bounded(
            &layout.shard_path(&shard_hash),
            limits
                .max_shard_bytes
                .min(MAX_SHARD_SIZE_BYTES as u64)
                .min(limits.max_read_bytes),
        )
        .await?;
    let mut read_bytes = body.len() as u64;
    let pointer_owned = pointer.clone();
    let worker_cancel = cancel.clone();
    let (mut proof, terms) = tokio::task::spawn_blocking(move || {
        parse_file(
            body,
            &pointer_owned,
            shard_hash,
            limits.max_chunks,
            &worker_cancel,
        )
    })
    .await??;
    let mut last_xorb: Option<(MerkleHash, XorbParser)> = None;
    for term in terms {
        check(cancel)?;
        if last_xorb
            .as_ref()
            .is_none_or(|(hash, _)| *hash != term.xorb_hash)
        {
            // Release the previous body before downloading the next one. This
            // keeps retained payload memory independent of the file's size.
            drop(last_xorb.take());
            let remaining = limits
                .max_read_bytes
                .checked_sub(read_bytes)
                .filter(|remaining| *remaining > 0)
                .ok_or(PointerProofError::Limit("origin read bytes"))?;
            let (body, _) = layout
                .store()
                .get_with_etag_bounded(
                    &layout.xorb_path(&term.xorb_hash),
                    limits
                        .max_xorb_bytes
                        .min(MAX_XORB_SIZE as u64)
                        .min(remaining),
                )
                .await?;
            read_bytes += body.len() as u64;
            let worker_cancel = cancel.clone();
            let parser = tokio::task::spawn_blocking(move || {
                check(&worker_cancel)?;
                let parser = XorbParser::parse(body)?;
                if parser.hash() != term.xorb_hash {
                    return Err(PointerProofError::Integrity("xorb identity mismatch"));
                }
                parser.verify_payload_digest()?;
                check(&worker_cancel)?;
                Ok(parser)
            })
            .await??;
            last_xorb = Some((term.xorb_hash, parser));
        }
        let (hash, parser) = last_xorb
            .take()
            .ok_or(PointerProofError::Integrity("missing verified xorb"))?;
        let worker_cancel = cancel.clone();
        (proof, last_xorb) =
            tokio::task::spawn_blocking(move || {
                for index in term.chunk_index_start..term.chunk_index_end {
                    check(&worker_cancel)?;
                    let (expected_hash, expected_size) =
                        proof.recipe.chunks.get(proof.cursor).ok_or(
                            PointerProofError::Integrity("recipe is shorter than its terms"),
                        )?;
                    let meta = parser.chunk_meta(index)?;
                    if meta.hash != *expected_hash
                        || u64::from(meta.uncompressed_len) != *expected_size
                    {
                        return Err(PointerProofError::Integrity(
                            "chunk metadata differs from shard recipe",
                        ));
                    }
                    let chunk = parser.get_chunk(index)?;
                    proof.hasher.update(&chunk.data);
                    proof.cursor += 1;
                }
                Ok((proof, Some((hash, parser))))
            })
            .await??;
    }
    check(cancel)?;
    if proof.cursor != proof.recipe.chunks.len() {
        return Err(PointerProofError::Integrity(
            "recipe is longer than its terms",
        ));
    }
    if proof.hasher.finalize().as_bytes() != &pointer.file_hash {
        return Err(PointerProofError::Integrity("whole-file hash mismatch"));
    }
    Ok(proof.recipe)
}

fn parse_file(
    body: Bytes,
    pointer: &Pointer,
    shard_hash: MerkleHash,
    max_chunks: usize,
    cancel: &CancellationToken,
) -> Result<(FileProof, Vec<ExtractedFileTerm>)> {
    check(cancel)?;
    if compute_data_hash(&body) != shard_hash {
        return Err(PointerProofError::Integrity("shard identity mismatch"));
    }
    let file_hash = MerkleHash::from(pointer.file_hash);
    let mut recipes =
        extract_file_recipes_for_hashes_with_limit(&body, &HashSet::from([file_hash]), max_chunks)?;
    if recipes.len() != 1 {
        return Err(PointerProofError::Integrity(
            "shard must contain exactly one matching file recipe",
        ));
    }
    let recipe = recipes
        .pop()
        .ok_or(PointerProofError::Integrity("missing file recipe"))?;
    let size = recipe
        .chunks
        .iter()
        .try_fold(0u64, |size, (_, len)| size.checked_add(*len))
        .ok_or(PointerProofError::Limit("recipe byte count"))?;
    if size != pointer.size {
        return Err(PointerProofError::Integrity(
            "recipe size differs from pointer",
        ));
    }
    // Replay the same bounded records instead of following the shard's lookup
    // offsets into a second, allocation-driven deserializer.
    let mut terms = Vec::new();
    visit_file_entries_from_reader(&mut std::io::Cursor::new(&body), |hash, entries| {
        if hash == file_hash {
            for term in entries {
                if terms.len() >= max_chunks {
                    return Err(std::io::Error::other("pointer proof term limit exceeded"));
                }
                terms.push(term);
            }
        }
        Ok(())
    })?;
    check(cancel)?;
    Ok((
        FileProof {
            recipe,
            cursor: 0,
            hasher: blake3::Hasher::new(),
        },
        terms,
    ))
}

fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(PointerProofError::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
