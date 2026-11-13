use super::*;
use std::io::{BufRead, BufReader, BufWriter};

/// Checkout line-ending behavior after Git resolves attributes and configuration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CheckoutEol {
    /// Keep the input's line endings.
    #[default]
    Preserve,
    /// Expand bare LF while preserving existing CRLF.
    CrLf,
    /// Expand only complete bodies that meet Git's automatic text rules.
    AutoCrLf,
}

/// Built-in checkout conversions over a verified raw blob.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Checkout {
    /// Resolved checkout line-ending policy.
    pub eol: CheckoutEol,
    /// Expand Git identifiers using the captured raw object's identity.
    pub ident: bool,
}

/// Copy verified local checkout bytes without capturing linear conversions.
///
/// Linear EOL conversion streams through the raw identity verifier. Ident and
/// automatic text rules use an authenticated seekable capture. Only absence
/// returns false; discard output on error and retain joined publication ownership.
pub fn copy_local_checkout(
    objects: &Path,
    oid: &str,
    size: u64,
    output: &mut impl Write,
    checkout: Checkout,
    scratch: impl FnOnce() -> io::Result<ScratchSpace>,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<bool> {
    if checkout == Checkout::default() {
        return copy_local_blob(objects, oid, size, output, scratch, cancelled);
    }
    if checkout.ident || checkout.eol == CheckoutEol::AutoCrLf {
        let scratch = scratch()?;
        let Some(mut captured) = capture_blob(objects, oid, size, &scratch, cancelled)? else {
            return Ok(false);
        };
        captured.copy_checkout(checkout, output, cancelled)?;
        return Ok(true);
    }
    // Forced EOL needs no global scan or replay. Authenticate the same input
    // while writing converted temporary bytes; full EOF still gates publication.
    let mut buffered = BufWriter::with_capacity(64 * 1024, output);
    let mut converted = LineEndings {
        output: &mut buffered,
        crlf: true,
        previous_cr: false,
    };
    let found = copy_local_blob(objects, oid, size, &mut converted, scratch, cancelled)?;
    buffered.flush()?;
    check_cancelled(cancelled)?;
    Ok(found)
}

impl CapturedBlob {
    fn copy_checkout(
        &mut self,
        checkout: Checkout,
        output: &mut impl Write,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        // Empty captures still authenticate their owned file length.
        self.copy_range(0..0, &mut io::sink(), cancelled)?;
        let oid = self.oid.clone();
        let mut input = BufReader::with_capacity(64 * 1024, Reader::new(self, cancelled)?);
        let ident = checkout.ident.then_some(oid.as_str());
        let crlf = match checkout.eol {
            CheckoutEol::Preserve => false,
            CheckoutEol::CrLf => true,
            CheckoutEol::AutoCrLf => {
                let mut stats = Stats::default();
                copy_ident(&mut input, &mut stats, ident, cancelled)?;
                input.rewind()?;
                stats.convert()
            }
        };
        let mut buffered = BufWriter::with_capacity(64 * 1024, output);
        let mut converted = LineEndings {
            output: &mut buffered,
            crlf,
            previous_cr: false,
        };
        copy_ident(&mut input, &mut converted, ident, cancelled)?;
        buffered.flush()?;
        check_cancelled(cancelled)
    }
}

struct Reader<'a> {
    blob: &'a mut CapturedBlob,
    cancelled: &'a dyn Fn() -> bool,
    position: u64,
    block: Option<u64>,
    bytes: Vec<u8>,
}

impl<'a> Reader<'a> {
    fn new(blob: &'a mut CapturedBlob, cancelled: &'a dyn Fn() -> bool) -> io::Result<Self> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(BLOCK_BYTES)
            .map_err(io::Error::other)?;
        Ok(Self {
            blob,
            cancelled,
            position: 0,
            block: None,
            bytes,
        })
    }
}

impl Read for Reader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        check_cancelled(self.cancelled)?;
        if output.is_empty() || self.position == self.blob.size {
            return Ok(0);
        }
        let block = self.position / BLOCK_BYTES as u64;
        if self.block != Some(block) {
            self.bytes.clear();
            let start = block * BLOCK_BYTES as u64;
            let end = self.blob.size.min(start + BLOCK_BYTES as u64);
            self.blob
                .copy_range(start..end, &mut self.bytes, self.cancelled)?;
            self.block = Some(block);
        }
        let offset = (self.position % BLOCK_BYTES as u64) as usize;
        let count = output.len().min(self.bytes.len() - offset);
        output[..count].copy_from_slice(&self.bytes[offset..offset + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for Reader<'_> {
    fn seek(&mut self, offset: SeekFrom) -> io::Result<u64> {
        let position = match offset {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.blob.size.checked_add_signed(offset),
        }
        .filter(|position| *position <= self.blob.size)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "checkout seek exceeds captured blob",
            )
        })?;
        self.position = position;
        Ok(position)
    }
}

fn copy_ident(
    input: &mut (impl BufRead + Seek),
    output: &mut impl Write,
    oid: Option<&str>,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<()> {
    let Some(oid) = oid else {
        loop {
            // io::copy retries Interrupted; cancellation must leave this owner
            // immediately instead of retrying an explicitly interrupted reader.
            check_cancelled(cancelled)?;
            let bytes = input.fill_buf()?;
            if bytes.is_empty() {
                return Ok(());
            }
            output.write_all(bytes)?;
            let count = bytes.len();
            input.consume(count);
        }
    };
    while copy_to_dollar(input, output, cancelled)? {
        output.write_all(b"$")?;
        let start = input.stream_position()?;
        let mut prefix = [0; 3];
        let mut count = 0;
        while count < prefix.len() {
            let read = input.read(&mut prefix[count..])?;
            if read == 0 {
                break;
            }
            count += read;
        }
        let substitute = count == 3 && prefix == *b"Id$"
            || count == 3 && prefix == *b"Id:" && expanded_ident(input, cancelled)?;
        if substitute {
            write!(output, "Id: {oid} $")?;
        } else {
            // A candidate may span an arbitrary number of bytes. Replay from
            // the owned seekable capture instead of retaining its whole text.
            input.seek(SeekFrom::Start(start))?;
        }
    }
    Ok(())
}

fn copy_to_dollar(
    input: &mut impl BufRead,
    output: &mut impl Write,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<bool> {
    loop {
        check_cancelled(cancelled)?;
        let bytes = input.fill_buf()?;
        if bytes.is_empty() {
            return Ok(false);
        }
        let position = bytes.iter().position(|byte| *byte == b'$');
        let count = position.unwrap_or(bytes.len());
        output.write_all(&bytes[..count])?;
        input.consume(count + usize::from(position.is_some()));
        if position.is_some() {
            return Ok(true);
        }
    }
}

fn expanded_ident(input: &mut impl BufRead, cancelled: &dyn Fn() -> bool) -> io::Result<bool> {
    let mut offset = 0;
    let mut space = false;
    loop {
        check_cancelled(cancelled)?;
        let bytes = input.fill_buf()?;
        if bytes.is_empty() {
            return Ok(false);
        }
        for (index, byte) in bytes.iter().copied().enumerate() {
            if byte == b'$' {
                input.consume(index + 1);
                return Ok(true);
            }
            if byte == b'\n' || space {
                return Ok(false);
            }
            // Git ignores the first byte after ':' and permits a final space.
            space = offset > 0 && byte == b' ';
            offset += 1;
        }
        let count = bytes.len();
        input.consume(count);
    }
}

#[derive(Default)]
struct Stats {
    printable: u64,
    nonprintable: u64,
    nul: bool,
    crlf: bool,
    lone_cr: bool,
    lone_lf: bool,
    previous_cr: bool,
    last: Option<u8>,
}

impl Stats {
    fn convert(&self) -> bool {
        let nonprintable = self.nonprintable - u64::from(self.last == Some(0x1a));
        self.lone_lf
            && !self.crlf
            && !self.lone_cr
            && !self.previous_cr
            && !self.nul
            && (self.printable >> 7) >= nonprintable
    }
}

impl Write for Stats {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for byte in bytes.iter().copied() {
            self.last = Some(byte);
            if self.previous_cr {
                self.previous_cr = false;
                if byte == b'\n' {
                    self.crlf = true;
                    continue;
                }
                self.lone_cr = true;
            }
            match byte {
                b'\r' => self.previous_cr = true,
                b'\n' => self.lone_lf = true,
                0 => {
                    self.nul = true;
                    self.nonprintable += 1;
                }
                8 | b'\t' | 27 | 12 => self.printable += 1,
                1..=31 | 127 => self.nonprintable += 1,
                _ => self.printable += 1,
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct LineEndings<W> {
    output: W,
    crlf: bool,
    previous_cr: bool,
}

impl<W: Write> Write for LineEndings<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.crlf {
            return self.output.write(bytes);
        }
        let mut start = 0;
        for (index, byte) in bytes.iter().copied().enumerate() {
            if byte == b'\n' && !self.previous_cr {
                self.output.write_all(&bytes[start..index])?;
                self.output.write_all(b"\r\n")?;
                start = index + 1;
            }
            self.previous_cr = byte == b'\r';
        }
        self.output.write_all(&bytes[start..])?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::process::{Command, Stdio};

    fn git(root: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
        let mut command = Command::new("git");
        command
            .current_dir(root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            command.env_remove(name);
        }
        let mut child = command.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let output = std::thread::scope(|scope| {
            scope.spawn(move || stdin.write_all(input).unwrap());
            child.wait_with_output().unwrap()
        });
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn checkout_matches_git_for_global_text_and_ident_rules() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("gitconfig"), "").unwrap();
        git(root.path(), &["init", "--template="], b"");
        let mut long = vec![b'x'; BLOCK_BYTES - 2];
        long.extend_from_slice(b"$Id:");
        long.extend(std::iter::repeat_n(b'x', BLOCK_BYTES + 9));
        long.extend_from_slice(b"$\n$Id$\n");
        let mut foreign = b"$Id: ".to_vec();
        foreign.extend(std::iter::repeat_n(b'x', BLOCK_BYTES + 5));
        foreign.extend_from_slice(b" foreign $\n$Id$\n");
        let bodies: &[&[u8]] = &[
            b"",
            b"a\nb\n",
            b"a\r\nb\n",
            b"a\rb\n",
            b"\0\na\n",
            b"a\n\x1a",
            b"$Id$\n",
            b"$Id: old $\n",
            b"$Id:foreign$\n",
            b"$Id: foreign value $\n",
            b"$Id: \0$\n",
            b"$Id: line\nbreak $ $Id$\n",
            b"$I$Id$ $Id:unfinished",
            &long,
            &foreign,
        ];
        for body in bodies {
            let (objects, oid) = super::super::tests::fixture(body);
            let stored = git(
                root.path(),
                &["hash-object", "--no-filters", "-w", "--stdin"],
                body,
            );
            assert_eq!(String::from_utf8(stored).unwrap().trim(), oid);
            let scratch = ScratchSpace::new(objects.path(), 4 * body.len() as u64 + 4096);
            for eol in [
                CheckoutEol::Preserve,
                CheckoutEol::CrLf,
                CheckoutEol::AutoCrLf,
            ] {
                for ident in [false, true] {
                    let text = match eol {
                        CheckoutEol::Preserve => "-text",
                        CheckoutEol::CrLf => "text eol=crlf",
                        CheckoutEol::AutoCrLf => "text=auto eol=crlf",
                    };
                    let attribute = if ident { "ident" } else { "-ident" };
                    std::fs::write(
                        root.path().join(".gitattributes"),
                        format!("*.txt {text} {attribute}\n"),
                    )
                    .unwrap();
                    let expected = git(
                        root.path(),
                        &["cat-file", "--filters", "--path=data.txt", &oid],
                        b"",
                    );
                    let mut converted = Vec::new();
                    assert!(
                        copy_local_checkout(
                            objects.path(),
                            &oid,
                            body.len() as u64,
                            &mut converted,
                            Checkout { eol, ident },
                            || Ok(scratch.clone()),
                            &|| false,
                        )
                        .unwrap()
                    );
                    assert_eq!(
                        converted,
                        expected,
                        "{eol:?}, ident={ident}, input={} bytes",
                        body.len()
                    );
                }
            }
        }
    }

    #[test]
    fn linear_checkout_does_not_require_capture_storage() {
        let body = b"a\r\nb\n";
        let (objects, oid) = super::super::tests::fixture(body);
        for eol in [CheckoutEol::Preserve, CheckoutEol::CrLf] {
            let checkout = Checkout { eol, ident: false };
            assert!(
                copy_local_checkout(
                    objects.path(),
                    &oid,
                    body.len() as u64,
                    &mut io::sink(),
                    checkout,
                    || Err(io::Error::other("capture storage unavailable")),
                    &|| false,
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn converted_output_rejects_corrupt_owned_bytes_and_interruption() {
        let bytes = vec![b'\n'; BLOCK_BYTES + 9];
        let (root, oid) = super::super::tests::fixture(&bytes);
        let scratch = ScratchSpace::new(root.path(), 4 * BLOCK_BYTES as u64);
        let mut blob = capture_blob(root.path(), &oid, bytes.len() as u64, &scratch, &|| false)
            .unwrap()
            .unwrap();
        let checkout = Checkout {
            eol: CheckoutEol::CrLf,
            ident: true,
        };
        assert_eq!(
            blob.copy_checkout(checkout, &mut io::sink(), &|| true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        blob.file.seek(SeekFrom::Start(0)).unwrap();
        blob.file.write_all(b"x").unwrap();
        let error = blob
            .copy_checkout(checkout, &mut io::sink(), &|| false)
            .unwrap_err();
        assert!(error.to_string().contains("block checksum differs"));
    }

    #[test]
    fn checkout_expands_the_captured_sha256_identity() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("gitconfig"), "").unwrap();
        git(
            root.path(),
            &["init", "--template=", "--object-format=sha256"],
            b"",
        );
        let bytes = b"$Id$\n$Id: old $\n";
        let oid = String::from_utf8(git(
            root.path(),
            &["hash-object", "--no-filters", "-w", "--stdin"],
            bytes,
        ))
        .unwrap();
        let oid = oid.trim();
        std::fs::write(
            root.path().join(".gitattributes"),
            "*.txt ident text eol=crlf\n",
        )
        .unwrap();
        let expected = git(
            root.path(),
            &["cat-file", "--filters", "--path=data.txt", oid],
            b"",
        );
        let scratch = ScratchSpace::new(root.path(), 4096_u64);
        let mut blob = capture_blob(
            &root.path().join(".git/objects"),
            oid,
            bytes.len() as u64,
            &scratch,
            &|| false,
        )
        .unwrap()
        .unwrap();
        let mut output = Vec::new();
        blob.copy_checkout(
            Checkout {
                ident: true,
                eol: CheckoutEol::CrLf,
            },
            &mut output,
            &|| false,
        )
        .unwrap();
        assert_eq!(output, expected);
    }

    #[test]
    fn empty_checkout_rejects_changed_owned_size() {
        let (root, oid) = super::super::tests::fixture(b"");
        let scratch = ScratchSpace::new(root.path(), 4096_u64);
        let mut blob = capture_blob(root.path(), &oid, 0, &scratch, &|| false)
            .unwrap()
            .unwrap();
        blob.file.write_all(b"changed").unwrap();
        assert_eq!(
            blob.copy_checkout(
                Checkout {
                    eol: CheckoutEol::AutoCrLf,
                    ident: false
                },
                &mut io::sink(),
                &|| false,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn checkout_preserves_interruption_after_reading_has_started() {
        let bytes = vec![b'\n'; BLOCK_BYTES + 9];
        let (root, oid) = super::super::tests::fixture(&bytes);
        let scratch = ScratchSpace::new(root.path(), 4 * BLOCK_BYTES as u64);
        let mut blob = capture_blob(root.path(), &oid, bytes.len() as u64, &scratch, &|| false)
            .unwrap()
            .unwrap();
        let polls = std::cell::Cell::new(0);
        let cancelled = || {
            let next = polls.get() + 1;
            polls.set(next);
            // A bounded pulse also makes an incorrectly retried interruption
            // terminate, so the regression fails without hanging its test process.
            (4..128).contains(&next)
        };
        let error = blob
            .copy_checkout(
                Checkout {
                    eol: CheckoutEol::AutoCrLf,
                    ident: false,
                },
                &mut io::sink(),
                &cancelled,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn checkout_matches_git_for_fragmented_identifier_tokens(
            tokens in proptest::collection::vec(proptest::sample::select(vec![
                &b"$Id$"[..], b"$Id: old $", b"$Id: foreign value $", b"$Id:\n",
                b"$I", b"d$", b"Id$", b"x ", b"\r", b"\n", b"\0", b"\x1a",
            ]), 0..48)
        ) {
            let bytes = tokens.concat();
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("gitconfig"), "").unwrap();
            git(root.path(), &["init", "--template="], b"");
            let stored = git(root.path(), &["hash-object", "--no-filters", "-w", "--stdin"], &bytes);
            let oid = String::from_utf8(stored).unwrap();
            std::fs::write(root.path().join(".gitattributes"), "*.txt ident text=auto eol=crlf\n").unwrap();
            let expected = git(root.path(), &["cat-file", "--filters", "--path=data.txt", oid.trim()], b"");
            let (objects, oid) = super::super::tests::fixture(&bytes);
            let scratch = ScratchSpace::new(objects.path(), bytes.len() as u64 + 4096);
            let mut blob = capture_blob(objects.path(), &oid, bytes.len() as u64, &scratch, &|| false).unwrap().unwrap();
            let mut output = Vec::new();
            blob.copy_checkout(Checkout { ident: true, eol: CheckoutEol::AutoCrLf }, &mut output, &|| false).unwrap();
            prop_assert_eq!(output, expected);
        }
    }
}
