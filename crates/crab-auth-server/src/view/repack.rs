use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::path::Path;
use std::pin::Pin;

use crab_git::{PointerKind, classify};
use crab_read::ShardHydrator;
use crab_types::pointer::Pointer;
use crab_xet::chunker::GearChunker;
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::{RunId, XorbBuilder, XorbResult};
use tokio_util::sync::CancellationToken;

use crate::error::{AuthServerError, Result};

pub(super) trait CrabPointerRewriter {
    fn rewrite<'a>(
        &'a mut self,
        pointer_bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + 'a>>;
}

pub(super) struct RepackedFile {
    pub(super) file_hash: MerkleHash,
    pub(super) size: u64,
    pub(super) chunk_hashes: Vec<MerkleHash>,
}

pub(super) struct ViewCrabObjects {
    pub(super) files: Vec<RepackedFile>,
    pub(super) xorbs: Vec<XorbResult>,
}

pub(super) struct ViewCrabRepacker {
    hydrator: ShardHydrator,
    builder: XorbBuilder,
    files: Vec<RepackedFile>,
    seen_files: HashMap<MerkleHash, usize>,
    next_run_id: u64,
}

impl ViewCrabRepacker {
    pub(super) fn new(hydrator: ShardHydrator) -> Self {
        Self {
            hydrator,
            builder: XorbBuilder::new(),
            files: Vec::new(),
            seen_files: HashMap::new(),
            next_run_id: 0,
        }
    }

    pub(super) fn finish(self) -> Result<ViewCrabObjects> {
        Ok(ViewCrabObjects {
            files: self.files,
            xorbs: self.builder.finalize()?,
        })
    }
}

impl CrabPointerRewriter for ViewCrabRepacker {
    fn rewrite<'a>(
        &'a mut self,
        pointer_bytes: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + 'a>> {
        Box::pin(async move {
            let pointer = Pointer::parse(pointer_bytes)?;
            let file_hash = MerkleHash::from(pointer.file_hash);
            if self.seen_files.contains_key(&file_hash) {
                return Ok(Pointer {
                    file_hash: pointer.file_hash,
                    size: pointer.size,
                    shard_hint: None,
                }
                .serialize());
            }

            // Keep unverified output operation-owned. The anonymous file is
            // released with its handles, including failed/cancelled reads.
            let cancel = CancellationToken::new();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let content = tempfile::tempfile()?;
            self.hydrator
                .reconstruct_to_writer_with_cancel(&pointer, content.try_clone()?, None, &cancel)
                .await
                .map_err(AuthServerError::from)?;
            let run_id = RunId(self.next_run_id);
            self.next_run_id = self.next_run_id.saturating_add(1);
            // Chunking/compression and temporary-file I/O must not block the
            // runtime. A failure aborts view creation before publication.
            let builder = std::mem::take(&mut self.builder);
            let (builder, chunk_hashes) = tokio::task::spawn_blocking(move || {
                repack_verified_file(content, builder, run_id, &cancel)
            })
            .await
            .map_err(|source| AuthServerError::ViewRepackJoin { source })??;
            self.builder = builder;

            self.seen_files.insert(file_hash, self.files.len());
            self.files.push(RepackedFile {
                file_hash,
                size: pointer.size,
                chunk_hashes,
            });

            Ok(Pointer {
                file_hash: pointer.file_hash,
                size: pointer.size,
                shard_hint: None,
            }
            .serialize())
        })
    }
}

fn repack_verified_file(
    mut content: File,
    mut builder: XorbBuilder,
    run_id: RunId,
    cancel: &CancellationToken,
) -> Result<(XorbBuilder, Vec<MerkleHash>)> {
    content.rewind()?;
    let mut buffer = [0; 64 * 1024];
    let mut chunker = GearChunker::new();
    let mut hashes = Vec::new();
    loop {
        if cancel.is_cancelled() {
            return Err(AuthServerError::from(crab_read::ReadError::Cancelled));
        }
        let len = content.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        for chunk in chunker.feed(&buffer[..len]) {
            hashes.push(chunk.hash);
            builder.push(&chunk, run_id)?;
        }
    }
    if let Some(chunk) = chunker.finalize() {
        hashes.push(chunk.hash);
        builder.push(&chunk, run_id)?;
    }
    Ok((builder, hashes))
}

pub(super) async fn materialize_crab_pointers_in_fast_export(
    input: &Path,
    output: &Path,
    rewriter: &mut dyn CrabPointerRewriter,
) -> Result<usize> {
    let input_file = File::open(input)?;
    let output_file = File::create(output)?;
    let mut reader = BufReader::new(input_file);
    let mut writer = BufWriter::new(output_file);
    let mut rewritten = 0usize;

    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(AuthServerError::Io)?;
        if read == 0 {
            break;
        }

        if line == b"blob\n" {
            writer.write_all(&line).map_err(AuthServerError::Io)?;
            rewritten += rewrite_blob_data_block(&mut reader, &mut writer, rewriter).await?;
            continue;
        }

        writer.write_all(&line).map_err(AuthServerError::Io)?;
        if let Some(size) = parse_fast_export_data_line(&line)? {
            copy_fast_export_data_block(&mut reader, &mut writer, size)?;
        }
    }

    writer.flush().map_err(AuthServerError::Io)?;
    Ok(rewritten)
}

async fn rewrite_blob_data_block<R, W>(
    reader: &mut R,
    writer: &mut W,
    rewriter: &mut dyn CrabPointerRewriter,
) -> Result<usize>
where
    R: BufRead,
    W: Write,
{
    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(AuthServerError::Io)?;
        if read == 0 {
            return Err(AuthServerError::AuthFailed {
                path: "fast-export blob ended before data block".into(),
            });
        }

        let Some(size) = parse_fast_export_data_line(&line)? else {
            writer.write_all(&line).map_err(AuthServerError::Io)?;
            continue;
        };

        let mut data = vec![0u8; size];
        reader.read_exact(&mut data).map_err(AuthServerError::Io)?;
        let had_lf = consume_optional_lf(reader)?;

        if matches!(classify(&data), PointerKind::Crab(_)) {
            let rewritten = rewriter.rewrite(&data).await?;
            writeln!(writer, "data {}", rewritten.len()).map_err(AuthServerError::Io)?;
            writer.write_all(&rewritten).map_err(AuthServerError::Io)?;
            writer.write_all(b"\n").map_err(AuthServerError::Io)?;
            return Ok(1);
        }

        writer.write_all(&line).map_err(AuthServerError::Io)?;
        writer.write_all(&data).map_err(AuthServerError::Io)?;
        if had_lf {
            writer.write_all(b"\n").map_err(AuthServerError::Io)?;
        }
        return Ok(0);
    }
}

fn copy_fast_export_data_block<R, W>(reader: &mut R, writer: &mut W, size: usize) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    let mut data = vec![0u8; size];
    reader.read_exact(&mut data).map_err(AuthServerError::Io)?;
    let had_lf = consume_optional_lf(reader)?;
    writer.write_all(&data).map_err(AuthServerError::Io)?;
    if had_lf {
        writer.write_all(b"\n").map_err(AuthServerError::Io)?;
    }
    Ok(())
}

fn consume_optional_lf<R: BufRead>(reader: &mut R) -> Result<bool> {
    let buffer = reader.fill_buf().map_err(AuthServerError::Io)?;
    if buffer.first() == Some(&b'\n') {
        reader.consume(1);
        Ok(true)
    } else {
        Ok(false)
    }
}

fn parse_fast_export_data_line(line: &[u8]) -> Result<Option<usize>> {
    let mut trimmed = line;
    if let Some(without_lf) = trimmed.strip_suffix(b"\n") {
        trimmed = without_lf;
    }
    if let Some(without_cr) = trimmed.strip_suffix(b"\r") {
        trimmed = without_cr;
    }
    let Some(size) = trimmed.strip_prefix(b"data ") else {
        return Ok(None);
    };
    let size = std::str::from_utf8(size)
        .map_err(|e| AuthServerError::AuthFailed {
            path: format!("fast-export data size is not UTF-8: {e}"),
        })?
        .parse::<usize>()
        .map_err(|e| AuthServerError::AuthFailed {
            path: format!("fast-export data size is invalid: {e}"),
        })?;
    Ok(Some(size))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn incremental_repack_preserves_chunk_order_across_read_boundaries() {
        let content: Vec<_> = (0..1_048_579_u32)
            .map(|offset| offset.wrapping_mul(2_654_435_761).rotate_left(offset % 32) as u8)
            .collect();
        let mut chunker = GearChunker::new();
        let mut chunks = chunker.feed(&content);
        chunks.extend(chunker.finalize());
        let expected: Vec<_> = chunks.iter().map(|chunk| chunk.hash).collect();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&content).unwrap();
        let (_, hashes) = repack_verified_file(
            file,
            XorbBuilder::new(),
            RunId(0),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(hashes, expected);
    }

    #[test]
    fn cancelled_repack_stops_before_reading_content() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = repack_verified_file(
            tempfile::tempfile().unwrap(),
            XorbBuilder::new(),
            RunId(0),
            &cancel,
        );
        assert!(matches!(
            result,
            Err(AuthServerError::Read(source)) if matches!(source.as_ref(), crab_read::ReadError::Cancelled)
        ));
    }

    struct FakeRewriter;

    impl CrabPointerRewriter for FakeRewriter {
        fn rewrite<'a>(
            &'a mut self,
            pointer_bytes: &'a [u8],
        ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + 'a>> {
            Box::pin(async move {
                match classify(pointer_bytes) {
                    PointerKind::Crab(pointer) => Ok(Pointer {
                        file_hash: pointer.file_hash,
                        size: pointer.size,
                        shard_hint: None,
                    }
                    .serialize()),
                    _ => Err(AuthServerError::AuthFailed {
                        path: "expected crab pointer".into(),
                    }),
                }
            })
        }
    }

    #[tokio::test]
    async fn materialize_crab_pointers_rewrites_only_blob_data() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("in.fast-export");
        let output = temp.path().join("out.fast-export");
        let pointer = format!(
            "version https://crab.build/spec/v1\nfile-hash {}\nsize 13\n",
            "1".repeat(64)
        );
        let stream = format!(
            "blob\nmark :1\ndata {}\n{}\ncommit refs/heads/main\nmark :2\ndata 5\nblob\n\nM 100644 :1 src/data.bin\n\n",
            pointer.len(),
            pointer
        );
        fs::write(&input, stream).unwrap();

        let mut rewriter = FakeRewriter;
        let rewritten = materialize_crab_pointers_in_fast_export(&input, &output, &mut rewriter)
            .await
            .unwrap();
        let out = fs::read(&output).unwrap();
        let text = String::from_utf8_lossy(&out);

        assert_eq!(rewritten, 1);
        assert!(text.contains("https://crab.build/spec/v1"));
        assert!(text.contains(&format!(
            "blob\nmark :1\ndata {}\n{}",
            pointer.len(),
            pointer
        )));
        assert!(text.contains("commit refs/heads/main\nmark :2\ndata 5\nblob\n\nM 100644"));
    }
}
