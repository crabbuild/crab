# Git capability matrix

This file is generated from `git-capability-matrix.json`. Run
`python3 crab/scripts/verify_git_capability_matrix.py --write` after editing the matrix.

| Profile | Git | OS | Provider | Operations | Status | Evidence |
| --- | --- | --- | --- | ---: | --- | --- |
| linux-rustfs-release | 2.30.9, 2.40.4, 2.45.4, current | linux | rustfs | 25 | supported | `.github/workflows/release.yml` / `protocol-v2-git-compatibility-release-gate` |
| linux-production-providers | 2.30.9, 2.40.4, 2.45.4, current | linux | aws_s3, gcs, azure_blob | 25 | preview | `.github/workflows/pb-provider-qualification.yml` / `provider-qualification` |
| non-linux-clients | 2.30.9, 2.40.4, 2.45.4, current | macos, windows | rustfs, aws_s3, gcs, azure_blob | 25 | preview | `.github/workflows/git-protocol-v2-partial-clone.yml` / `cross-platform-contract` |

## Protocol boundary

Transport: `local stateless-connect git-upload-pack`.

Supported: `protocol_v2_ls_refs`, `protocol_v2_fetch`, `complete_pack_fallback`, `shallow_and_deepen`, `filter_blob_none`, `filter_blob_limit`, `filter_tree_depth`, `filter_sparse_oid`, `filter_object_type`.

Unsupported: `stateful_connect`, `git_receive_pack_connect`, `packfile_uris`, `object_info`, `ref_in_want`, `deepen_since`, `deepen_not`.

Unsupported v2 requests are rejected before pack bytes. Helper transport negotiation may return `fallback` before handoff (for example, receive-pack takeover uses the ordinary helper push path); Crab never substitutes a complete fetch for a rejected v2 request or falls back after partial v2 output.

`supported` declares mandatory release-gate cells, not a claim that an unverified checkout passed. The named workflow must validate a fresh report against these operation checks, the exact packaged binary, clean source SHA, Git executable, platform, provider, and pinned rollback binary. Missing or skipped checks fail the gate. `preview` is not a compatibility promise.
