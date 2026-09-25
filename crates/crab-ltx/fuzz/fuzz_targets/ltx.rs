#![no_main]

//! Any byte string must decode or fail, never panic.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = crab_ltx::internal::inspect_ltx(data);
    let _ = crab_ltx::internal::cut_upper_bound(4096, data.len() as u64);
});
