# Isolated decoder performance experiment

Source: rustls-pki-types 1.14.1, upstream revision
`bb3c1da0e69e8ee3dcdfb16c245f974d636ed481`.
The original registry archive SHA-256 is
`30a7197ae7eb376e574fe940d068c30fe0462554a3ddbe4eca7838e049c937a9`.

Only `src/base64.rs` differs from the published source. Public PEM decoding
collects complete ordinary base64 blocks before scalar padding/whitespace
handling. Secret PEM data retains the scalar path. Result bytes, error priority,
and partial output writes must match the published decoder.

This experiment changes no TLS settings, trust stores, credentials, or client
construction boundaries. The override is not qualified for retention until
normal release push comparisons show a repeatable gain and dependency, TLS,
platform, and constant-time checks pass. Revert it if that proof fails.

Original licenses, manifests, source files, and data remain included.

Qualification uses the pinned upstream checkout with this `src/base64.rs`
copied into it. The published archive omits the integration tests and PEM
fixtures needed by its doctests. The isolated workflow job restores those
fixtures, runs upstream tests, checks Rust 1.60, and executes the original
secret-codepoint side-channel test under Valgrind. A decoder regression also
compares public/scalar results, errors, and complete partial-output buffers.
