# `crab-ltx` fuzz targets

One target per untrusted decoder. Every target drives the stable inspection
surface in `crab_ltx::internal`, which is the same code production runs; a
panic, hang, or unbounded allocation is a finding.

| Target | Surface |
| --- | --- |
| `ltx` | LTX header, page frames, index, trailer, and rolling checksums |
| `root` | Root document JSON and root descriptor pages |
| `directory` | Authenticated radix directory nodes |
| `bundle` | Bundle envelope, footer, and rows |
| `node_frame` | Authenticated node-log frame |

## Run

Fuzzing needs a nightly toolchain and `cargo-fuzz`:

```sh
cargo install cargo-fuzz
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ltx-fuzz" \
  cargo +nightly fuzz run ltx -- -max_total_time=600
```

The external vectors in `../tests/vectors/` seed every corpus:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ltx-fuzz" \
  cargo +nightly fuzz run ltx ../tests/vectors
```

## Stable-toolchain replay

CI does not require nightly. `../tests/ltx/vectors.rs` runs the same entry points
over every truncation, deterministic bit flip, and length prefix of the external
vectors, so the decoders stay total on the stable toolchain and a panic fails
`cargo test -p crab-ltx --features replica`. Keep the replay and these targets in
step when adding a decoder.
