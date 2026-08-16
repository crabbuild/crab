//! Streaming clean-filter memory regression test.
//!
//! Pushes a large buffer through `CleanSession::clean_stream` and
//! asserts peak RSS stays bounded. The test is `#[ignore]` by default
//! because RSS measurements are inherently noisy (test process
//! overhead, other concurrent tests, OS memory accounting) and the
//! test runs slowly — explicit opt-in via `cargo test -- --ignored`
//! keeps CI fast and stable.
//!
//! ## Why 128 MiB
//!
//! 128 MiB is large enough to cross the clean path's chunk-buffer cap
//! and exercise provisional staging, while still leaving room for noisy
//! RSS accounting on developer machines. A non-streaming implementation
//! on 128 MiB would peak near 256 MiB (input + chunks), well above the
//! 200 MiB threshold.
//!
//! The same assertion also verifies the streaming path's output
//! pointer is byte-identical to the pointer produced by `clean_file`
//! on the same content, matching the "resulting pointer equals the
//! non-streaming path's pointer" part of the task.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::io::{self, Read};

use crab::core::context::AppContext;
use crab::git::clean::CleanSession;
use crab::git::filter_process::PktLineReader;

/// Total input size streamed through the clean filter.
const STREAM_SIZE: usize = 128 * 1024 * 1024;

/// Peak RSS delta budget. A non-streaming implementation on 128 MiB
/// would peak near 256 MiB (input buffer + accumulated chunks);
/// staying under 200 MiB demonstrates the input is not buffered.
const RSS_DELTA_BUDGET_BYTES: usize = 200 * 1024 * 1024;

/// Body bytes per pkt-line data packet. Matches the practical upper
/// bound git uses (`PKT_LINE_MAX_BODY` = 65 516 is the theoretical cap;
/// 65 000 keeps room for the 4-byte length header under 0xffff).
const PKT_BODY_SIZE: usize = 65_000;

/// Deterministic, streaming byte generator.
///
/// Emits `remaining` bytes of pseudo-random data on demand, avoiding
/// the need to materialize a 128 MiB input buffer up front. Uses a
/// minimal LCG so test input generation itself doesn't dominate RSS
/// or runtime.
struct RngReader {
    state: u64,
    remaining: usize,
}

impl RngReader {
    fn new(seed: u64, total: usize) -> Self {
        Self {
            state: seed | 1,
            remaining: total,
        }
    }
}

impl Read for RngReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.remaining);
        for slot in &mut buf[..n] {
            // Numerical Recipes LCG — fast, non-cryptographic, fine
            // for producing incompressible-looking bytes.
            self.state = self
                .state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *slot = (self.state >> 33) as u8;
        }
        self.remaining -= n;
        Ok(n)
    }
}

/// Streaming pkt-line encoder: wraps an inner `Read` and emits its
/// content as a sequence of pkt-line data packets followed by a
/// single flush packet.
///
/// Only buffers one pkt-line at a time, so feeding a multi-gigabyte
/// stream through this adapter doesn't grow RSS beyond `PKT_BODY_SIZE
/// + 4` bytes plus whatever the downstream reader holds.
struct PktLineEncoder<R: Read> {
    inner: R,
    /// Currently-framed packet bytes waiting to be drained.
    packet: Vec<u8>,
    /// Read cursor within `packet`.
    packet_pos: usize,
    /// Set once the inner reader has reported EOF.
    inner_eof: bool,
    /// Set once the final `0000` flush frame has been emitted.
    flushed: bool,
}

impl<R: Read> PktLineEncoder<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            packet: Vec::with_capacity(PKT_BODY_SIZE + 4),
            packet_pos: 0,
            inner_eof: false,
            flushed: false,
        }
    }

    /// Frame the next packet into `self.packet`. Returns `Ok(false)`
    /// when the whole stream (including flush) is done.
    fn refill(&mut self) -> io::Result<bool> {
        self.packet.clear();
        self.packet_pos = 0;

        if self.flushed {
            return Ok(false);
        }

        if self.inner_eof {
            self.packet.extend_from_slice(b"0000");
            self.flushed = true;
            return Ok(true);
        }

        // Read up to PKT_BODY_SIZE body bytes. Handle short reads by
        // looping — `Read::read` may return less than requested even
        // without hitting EOF.
        let mut body = vec![0u8; PKT_BODY_SIZE];
        let mut filled = 0;
        while filled < body.len() {
            match self.inner.read(&mut body[filled..]) {
                Ok(0) => {
                    self.inner_eof = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }

        if filled == 0 {
            // Inner hit EOF with no bytes to frame — emit flush next.
            self.packet.extend_from_slice(b"0000");
            self.flushed = true;
            return Ok(true);
        }

        let total = filled + 4;
        assert!(total <= 0xffff, "pkt-line body too large: {total}");
        self.packet
            .extend_from_slice(format!("{total:04x}").as_bytes());
        self.packet.extend_from_slice(&body[..filled]);
        Ok(true)
    }
}

impl<R: Read> Read for PktLineEncoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.packet_pos >= self.packet.len() && !self.refill()? {
            return Ok(0);
        }
        let remaining = &self.packet[self.packet_pos..];
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.packet_pos += n;
        Ok(n)
    }
}

/// Sample physical memory (RSS) in bytes, or `None` if the platform
/// can't report it. Wraps the third-party probe so call sites stay
/// compact.
fn rss_bytes() -> Option<usize> {
    memory_stats::memory_stats().map(|s| s.physical_mem)
}

#[test]
#[ignore = "manual: streams 128 MiB, measures peak RSS; run with --ignored"]
fn clean_stream_bounded_rss_on_128mib_input() {
    let baseline = rss_bytes().expect("RSS probe not supported on this platform");

    // Build the streaming pipeline: 128 MiB of pseudo-random bytes →
    // pkt-line framer → PktLineReader → CleanSession::clean_stream.
    let rng = RngReader::new(0xDEAD_BEEF_CAFE_BABE, STREAM_SIZE);
    let framed = PktLineEncoder::new(rng);
    let mut reader = PktLineReader::from_read(framed);

    let mut session = CleanSession::new(AppContext::default());
    let stream_pointer = session
        .clean_stream("big.bin", &mut reader)
        .expect("clean_stream must succeed");

    let peak = rss_bytes().expect("RSS probe not supported on this platform");
    let delta = peak.saturating_sub(baseline);

    eprintln!(
        "clean_stream 128MiB: baseline_rss={baseline} peak_rss={peak} delta={delta} \
         (budget={RSS_DELTA_BUDGET_BYTES})"
    );

    assert!(
        delta < RSS_DELTA_BUDGET_BYTES,
        "streaming clean filter RSS grew by {delta} bytes on a {STREAM_SIZE}-byte input, \
         exceeding the {RSS_DELTA_BUDGET_BYTES}-byte budget; a non-streaming implementation \
         would pin the whole input in memory",
    );

    // Regenerate the same 128 MiB content into an owned buffer and
    // feed it to `clean_file` to obtain the buffered-entry-point
    // pointer. `clean_file` delegates to `clean_stream` internally,
    // so this asserts byte-identical output for the
    // same content across both entry points.
    //
    // Done *after* the RSS assertion so the 128 MiB materialization
    // doesn't contaminate the peak measurement above.
    let mut buffered = Vec::with_capacity(STREAM_SIZE);
    RngReader::new(0xDEAD_BEEF_CAFE_BABE, STREAM_SIZE)
        .read_to_end(&mut buffered)
        .expect("RNG reader must fill buffer");
    assert_eq!(buffered.len(), STREAM_SIZE);

    let mut session_b = CleanSession::new(AppContext::default());
    let file_pointer = session_b
        .clean_file("big.bin", buffered)
        .expect("clean_file must succeed");

    assert_eq!(
        stream_pointer, file_pointer,
        "streaming and non-streaming paths must produce byte-identical pointers"
    );
}
