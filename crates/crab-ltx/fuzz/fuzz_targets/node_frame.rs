#![no_main]

//! Authenticated node frames must reject malformed scope and body fields.

use crab_ltx::Limits;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = Limits {
        max_capture_bytes: 64 * 1024,
        max_file_bytes: 256 * 1024,
        max_plan_bytes: 1024 * 1024,
        ..Limits::default()
    };
    let _ = crab_ltx::internal::inspect_node_frame(data, limits);
});
