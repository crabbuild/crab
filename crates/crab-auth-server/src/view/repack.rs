use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::pin::Pin;

use crab_git::{PointerKind, classify};
use crab_read::ShardHydrator;
use crab_types::pointer::Pointer;
use crab_xet::chunker::GearChunker;
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::{RunId, XorbBuilder, XorbResult};

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

            let content = self
                .hydrator
                .reconstruct_from_pointer(pointer_bytes)
                .await
                .map_err(super::read_error)?;
            if content.len() as u64 != pointer.size {
                return Err(AuthServerError::CorruptObject {
                    path: file_hash.hex(),
                    reason: format!(
                        "source pointer size {} does not match reconstructed size {}",
                        pointer.size,
                        content.len()
                    ),
                });
            }
            let computed_hash = MerkleHash::from(*blake3::hash(&content).as_bytes());
            if computed_hash != file_hash {
                return Err(AuthServerError::HashMismatch {
                    requested: file_hash.hex(),
                    actual: computed_hash.hex(),
                });
            }

            let mut chunker = GearChunker::new();
            let mut chunks = chunker.feed(&content);
            if let Some(last) = chunker.finalize() {
                chunks.push(last);
            }
            let chunk_hashes: Vec<MerkleHash> = chunks.iter().map(|chunk| chunk.hash).collect();
            let run_id = RunId(self.next_run_id);
            self.next_run_id = self.next_run_id.saturating_add(1);
            for chunk in &chunks {
                self.builder.push(chunk, run_id)?;
            }

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
            "version https://crab.dev/spec/v1\nfile-hash {}\nsize 13\n",
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
        assert!(text.contains("https://crab.dev/spec/v1"));
        assert!(text.contains(&format!(
            "blob\nmark :1\ndata {}\n{}",
            pointer.len(),
            pointer
        )));
        assert!(text.contains("commit refs/heads/main\nmark :2\ndata 5\nblob\n\nM 100644"));
    }
}
