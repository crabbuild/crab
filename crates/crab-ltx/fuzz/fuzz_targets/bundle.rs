#![no_main]

//! Bundle envelopes must reject malformed footers and rows within their limits.

use crab_ltx::Limits;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // A small bound keeps the target focused on parsing, not on allocation.
    let limits = Limits {
        max_capture_bytes: 64 * 1024,
        max_file_bytes: 256 * 1024,
        max_plan_bytes: 1024 * 1024,
        ..Limits::default()
    };
    let _ = crab_ltx::internal::inspect_bundle(data, limits);
});
