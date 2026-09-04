//! Bounded whole-request admission for line-delimited LFS operands.

use std::io::{self, BufRead, Read};
use tokio_util::sync::CancellationToken;

use crate::core::error::{Result, check_cancelled};
use crate::git::process::MAX_CAPTURE_BYTES;

const MAX_LINE_BYTES: u64 = 1024 * 1024;

pub(super) fn read_stdin_lines(cancel: &CancellationToken) -> Result<Vec<String>> {
    read_lines(std::io::stdin().lock(), MAX_CAPTURE_BYTES, cancel)
}

fn read_lines(
    mut input: impl BufRead,
    limit: u64,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let mut values = Vec::new();
    let mut line = Vec::new();
    let mut remaining_input = limit;
    let mut remaining_inventory = limit;
    loop {
        check_cancelled(cancel)?;
        line.clear();
        let available = remaining_input.min(MAX_LINE_BYTES);
        let read = input
            .by_ref()
            .take(available.saturating_add(1))
            .read_until(b'\n', &mut line)?;
        check_cancelled(cancel)?;
        if read == 0 {
            return Ok(values);
        }
        if read as u64 > available {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LFS stdin exceeds the input or line limit",
            )
            .into());
        }
        remaining_input -= read as u64;
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        let value = std::str::from_utf8(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if value.is_empty() {
            continue;
        }
        if value.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control byte in LFS stdin operand",
            )
            .into());
        }
        // Keep operand bytes exact. Trimming would turn malformed input into
        // a different revision; empty lines and CRLF are framing, not operands.
        let retained = (std::mem::size_of::<String>() + value.len()) as u64;
        remaining_inventory = remaining_inventory.checked_sub(retained).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "LFS stdin exceeds the inventory limit",
            )
        })?;
        values.push(value.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::error::CrabError;

    #[test]
    fn operands_preserve_bytes_and_accept_crlf_and_final_eof() {
        let values = read_lines(
            &b"main\r\n\n main \nHEAD~1"[..],
            256,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(values, ["main", " main ", "HEAD~1"]);
    }

    #[test]
    fn invalid_encoding_and_control_bytes_reject_the_entire_input() {
        for input in [b"main\n\xff\n".as_slice(), b"main\nHEAD\0\n", b"main\r"] {
            assert!(matches!(
                read_lines(input, 256, &CancellationToken::new()),
                Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::InvalidData
            ));
        }
    }

    #[test]
    fn raw_input_and_retained_inventory_have_independent_limits() {
        let cancel = CancellationToken::new();
        assert!(read_lines(&b"\n\n\n\n"[..], 4, &cancel).unwrap().is_empty());
        assert!(read_lines(&b"\n\n\n\n\n"[..], 4, &cancel).is_err());
        assert_eq!(read_lines(&b"a\nb\n"[..], 64, &cancel).unwrap(), ["a", "b"]);
        assert!(matches!(
            read_lines(&b"a\nb\nc\n"[..], 64, &cancel),
            Err(CrabError::Io(error)) if error.to_string().contains("inventory limit")
        ));
    }

    #[test]
    fn one_oversized_line_fails_before_reading_unbounded_input() {
        let input = vec![b'a'; MAX_LINE_BYTES as usize + 1];
        assert!(matches!(
            read_lines(input.as_slice(), MAX_CAPTURE_BYTES, &CancellationToken::new()),
            Err(CrabError::Io(error)) if error.to_string().contains("line limit")
        ));
    }

    #[test]
    fn read_errors_and_precancellation_are_preserved() {
        struct Failed;
        impl Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
        }
        let cancel = CancellationToken::new();
        assert!(matches!(
            read_lines(io::BufReader::new(Failed), 256, &cancel),
            Err(CrabError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
        ));
        cancel.cancel();
        assert!(matches!(
            read_lines(io::BufReader::new(Failed), 256, &cancel),
            Err(CrabError::Cancelled)
        ));
    }
}
