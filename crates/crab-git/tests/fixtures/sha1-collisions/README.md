SHA-mbles chosen-prefix collision fixtures from the MIT/Apache-2.0 licensed
[`sha1-checked` 0.10.0 source](https://crates.io/crates/sha1-checked/0.10.0),
`tests/data/sha-mbles-{1,2}.bin`. Original attack and inputs:
https://sha-mbles.github.io/.

These are mandatory collision-detection regression inputs, not Git objects.
Tests check rejection and the unmodified SHA-1 digest across update boundaries.

SHA-256:

- `sha-mbles-1.bin`: `3ead211681cec93d265c8ac123dd062e105408cebf82fa6e2b126f4f40bcb88c`
- `sha-mbles-2.bin`: `208feafe1c6a95c73f662514ac48761f25e1f3b74922521a98d9ce287f4a2197`
