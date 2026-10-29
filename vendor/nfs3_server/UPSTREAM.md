# nfs3_server 0.11.0

Source: https://github.com/Vaiz/nfs3, commit
`1c0681fae059ac544f2e4a7756051b2c261f60df`, `crates/nfs3_server`.
Vendored from the locked crates.io 0.11.0 package under its BSD-3-Clause license.
The normalized package manifest, README, license and complete source are retained.
README trailing whitespace is normalized; license text remains byte-identical.
Registry markers and the package's standalone lockfile are not build inputs.

The scoped ownership patch adds graceful listener shutdown, joins connections,
parsers and admitted RPCs, and runs transaction cleanup within the listener.
Crab must await graceful shutdown after kernel detachment. Wire formats,
transaction limits and dependency versions remain unchanged.

This dependency patch and its Cargo manifest/lockfile changes were explicitly
approved for Crab's NFS lifecycle repair. No other dependency patch is included.
