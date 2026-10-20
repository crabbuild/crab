# Cloudflare / R2 evaluation smoke — 2026-10-20

This is an evaluation receipt, not native release qualification.

| Identity | Value |
| --- | --- |
| Server source | `8a791932ea752a1d3ce69ca9efde2204c79af86b` |
| Linux image CI | [39460182791](https://github.com/crabbuild/crab/actions/runs/39460182791) |
| CI image config digest | `sha256:d89cac10f3786edc33d30718ac9f230e5f74002006f541293caa9d58930c4e8a` |
| Hosted wrapper manifest digest | `sha256:6d36277ee186fbb94a522ae113e6c072c9479741bfc6e145150d6264f51cec6c` |
| Runtime allocation | 1 vCPU, 3072 MiB memory, 20000 MB ephemeral disk; one instance |
| Public application | `https://crab.fernbase.dev/crabbuild/crab` |
| Private R2 bucket | `crab-http-server-native-20261020` |
| Imported Git branch | `crab-v2` at `19d503ecc1f47ac183448b5602bd860e1b46c466` |

The dedicated Access application allows only the account owner's email. A
temporary service-token policy enabled automated verification. The runtime uses
bucket-scoped R2 credentials; the account administration token stays local.

Passed:

- R2 read/list/write, conditional publication and coordination preflight.
- Linux amd64 image build, vulnerability scan, isolated native process restart.
- Live Cloudflare application resource allocation verified through its API.
- Anonymous HTTPS requests redirected to Access; service-authenticated requests
  passed Access and the Worker's JWT verification.
- Fresh hosted startup restored the catalog, native Cell, and original Git refs
  from R2 after the local bootstrap process was drained.
- Browser code/README browsing, issue #2 creation and comment creation.
- Browser file commit to `cloudflare-verification`, published as
  `867d2cf4c31f95c42661957acd67155863635e60`, with the comparison showing one file.
- Native HTTPS Git ref advertisement matched the imported source branch.
- LFS upload/download, SHA-256 identity, byte-range download, and public HTTPS
  batch action URLs.
- SIGTERM → observed stopped state → healthy cold start. Git refs, both issues,
  browser comment, committed file bytes, and LFS bytes survived. Replaying the
  original issue submission still returned issue #1.

Folder last-commit attribution returned HTTP 202 `indexing` during the smoke;
that path has not been qualified. Human OTP completion, native multi-user
authorization, multi-replica handoff, abrupt crashes, restore, load, and upgrade
matrices were not exercised. The [Cellar release blockers](../../CELLAR.md)
remain open.
