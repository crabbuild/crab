# Object Store Layout: Evidence Audit

Companion to [Repository Object Store Key Layout](object-store-key-layout.md).
Audited source: `e26d139038414dcb8ddc591712d726f052547131`.
Original RustFS inspection: `2026-09-03T05:20:24.891135+00:00`.
Follow-up LIST: `2026-09-03T06:17:44.377640+00:00`.

## Evidence rules

The design document combines an implementation inventory with a proposal.
These are different evidence classes:

- **Source fact:** a named writer, reader, validator, or constant implements
  the stated behavior at the pinned revision. Links below pin the revision
  and starting line; read the complete named function, including its callees.
- **Measurement:** saved LIST metadata, bounded object reads, pack headers,
  or the original full-stream hash result. Measurements apply to their
  recorded timestamp, not indefinitely to the live bucket.
- **Inference:** a conclusion connecting specific source paths or measurements.
  It is labeled as an inference where no E2E reproduction was performed.
- **Proposal:** a recommended invariant, optimization, migration rule, or
  acceptance criterion. It is a design judgment, not a claim of current
  conformance, measured savings on every workload, or an approved migration.
- **Open proof:** the named search or source comparison did not establish a
  required behavior. This does not prove that data loss occurred.

Broad claims cannot be justified by a filename alone. The audit followed key
constructors into their writers/readers, checked publication/cleanup callers,
and compared preview, durable GC, service, and feature owners where they share
a dependency. Existing tests are supporting specifications; this audit did
not execute Rust tests or turn source evidence into an E2E certification.

## Corrections made during the audit

| Earlier wording or omission | Correction and evidence |
| --- | --- |
| ACL views named by the manifest validation digest | The caller passes `snapshot.journal.state_digest`: [materialization caller](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/view.rs#L194) → [view_prefix](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/view.rs#L763). |
| Shallow descriptor listed without its entry key family | Added `metadata/shallow-closure/entries/{hash}.bin`: [entry constructor](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/shallow_closure.rs#L292) and [publication](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/shallow_closure.rs#L376). |
| LFS objects/locks presented without verification receipts | Added `lfs/receipts/{aa}/{bb}/{sha256}.bin`, overwrite/binding behavior, and the body-only delete boundary: [receipt reader/constructor](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-lfs/src/object_store.rs#L841), [delete](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-lfs/src/object_store.rs#L542). |
| Generic admission clock described as below a slot | Push admission probes `slots/clock`, using the slots prefix: [backend_now](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/push_admission.rs#L417), [prefix](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/push_admission.rs#L453). |
| Closure segment described as at most 4,096 hashes total | The publisher slices each of its xorb and file lists separately, up to 4,096 each: [publish](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/closure.rs#L191). |
| Push-lease expiry description omitted a reader branch | Non-released payloads with `lease_secs == 0` use `expires_at`; push admission has a different helper: [authoritative_expiry](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/push_lock.rs#L744), [admission implementation](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/push_admission.rs#L453). |
| Active-session handling was too broad | Cleanup skips this context's prefix and two session keys; it does not enumerate all other sessions' activity: [cleanup_expired_staging](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/receive/session.rs#L232). |
| No standalone flat shard-list reader implied | The reachable shard-compaction command reads and CAS-updates `manifests/shard-list`: [CLI caller](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/main.rs#L5579), [reader](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/compact.rs#L328), [writer](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/compact.rs#L291). |
| Lock cost attributed primarily to requests | Removed: there is no request-count or billing measurement. Kept measured object count and bytes. |
| Original cache inventory could be mistaken for current state | Follow-up LIST contains zero generated-pack objects. The original 235-object snapshot remains historical evidence, with a separately timestamped 164-object listing. |

## Claim-to-source map

The table covers the factual groups in the design document. The requirement
and decision tables in sections 1, 10.2, 12–15 are proposed policy, except for
the specifically sourced current behavior and measurements cited there.

| Design section / factual group | Evidence and owner boundary |
| --- | --- |
| 2: independent `R`/`G`, scoped overrides, default `.crab` | [StoreLayout::new](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-storage/src/layout.rs#L99), [StorageScope](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-types/src/storage.rs#L78); service view placement uses [materialize_view_with_store_and_credentials](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/view.rs#L194). This is not proof that every independent metadata configuration propagates scoped roots. |
| 3: key bytes and SDK conversion | [layout constructors](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-storage/src/layout.rs#L99) use `ObjectPath::from`; pinned dependency source and its documented vectors are recorded below. Rejection/no-normalization rules are the proposed contract, not universal current enforcement. |
| 3: hash identities and fixed-width generation paths | [layout keys](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-storage/src/layout.rs#L169), [transaction ID](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/ref_journal.rs#L82), [repository/root partition hashes](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/ref_registry.rs#L361), [request identity](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-remote-git/src/pack.rs#L394), [artifact version identity](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-workflow/src/artifact.rs#L1936). These hashes have different preimages. |
| 4–5: physical core keys | [StoreLayout key constructors](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-storage/src/layout.rs#L169), [segment/index constructors](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/segmented.rs#L129), [split graph publisher](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/split_commit_graph.rs#L823), [shallow entry constructor](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/shallow_closure.rs#L292). Feature rows have separate owners below. |
| 5.1: descriptor schema and canonical constants | [descriptor constants/validation](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/layout_descriptor.rs#L7), called by [remote-layout open](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/core/remote_layout.rs#L22). DB path constructors, not the descriptor's partition-bit names, determine physical DB roots. |
| 5.1, 10.1: coherent snapshot, manifest history, frontier ordering | [snapshot reader](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/manifest_store.rs#L322), [journal compaction](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/manifest_store.rs#L361), [manifest CAS and archive call](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/manifest_store.rs#L642). |
| 5.1, 10.1: journal prepare, commit marker, promotion, cleanup | [commit_ref_transaction](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/ref_journal.rs#L201), [cleanup_compacted_transactions](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/ref_journal.rs#L375). The marker publication precedes best-effort head promotion. |
| 5.2: visibility formats and pending recovery dependencies | [read_with_format](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_visibility.rs#L2808), [prepare_catalog_journal_edits](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_visibility.rs#L3137), [apply_catalog_journal_edits](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_visibility.rs#L3281). The catalog reader depends on a matching database checkpoint. |
| 5.2: generation receipts | CLI `write_generation_index_receipt` / receipt construction in [metadb.rs](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/metadb.rs#L2129) and service `write_service_generation_index_receipt` in [receive.rs](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/receive.rs#L1978); both construct generation-width paths and validate conflicting coverage. |
| 5.2: optional bulk ref-registry metadata | [mark visitor](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/mod.rs#L1681), [streaming visitor](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/mod.rs#L2176), and [history dependency reader](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/history_recovery.rs#L725) recognize the field. Searches for `ref_registry_hash` assignments and `bulk_manifest_path("ref-registry"` found no production publisher in `crab/src` or `crates`; this is a bounded search result. |
| 5.2: replica-discovery mutation | [publish](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/replication/discovery.rs#L22) uses `put_overwrite`; `load` validates the document against the expected primary. No ref publication is performed by this file. |
| 6.1: canonical pack family, derived metadata, origin proof | [pack paths](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-storage/src/layout.rs#L169), [upsert_pack_metadata](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/git/push.rs#L120), [record_verified_pack_origin](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/pack_origin.rs#L86); [service pack publication](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/receive/git_workspace.rs#L472) writes pack/kind/meta objects. |
| 6.2: generated request fields, version, artifact publication | [request/selection hashes](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-remote-git/src/pack.rs#L394), [publish_cached_pack](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-remote-git/src/pack.rs#L2014), [load_cached_pack](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-remote-git/src/pack.rs#L1807); `GENERATED_PACK_CACHE_VERSION = 3` is in the same module. |
| 6.2, 12: exact canonical-pack reuse still publishes a cache body | [try_reuse_single_pack](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-remote-git/src/pack.rs#L2133) returns verified bytes; [cache publisher](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-remote-git/src/pack.rs#L2014) addresses the generated artifact key. The proposal to reference canonical storage is not implemented by this PR. |
| 6.2: caller coverage | [remote helper](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/git/remote_helper.rs#L2615) calls `generate_pack_cached`; [wire request cache](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/git/upload_pack_wire.rs#L1762) and [wire selection cache](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/git/upload_pack_wire.rs#L1961) use the shared implementation. |
| 6.2: cache age and default grace | [cache reachability](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/mod.rs#L2025) uses `last_modified` and `max(MIN_GRACE_PERIOD)`; [configuration default](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/core/config.rs#L940) is 24 hours. No last-access refresh is performed by the inspected cache-hit reader. |
| 7.1: global registry and closure grammar | [registry constructors](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/ref_registry.rs#L361), [closure publication](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/closure.rs#L191); registry CAS/repair routines follow those constructors in the same module. |
| 7.2: physical database paths | [MetaDbConfig defaults](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/metadata/metadb/mod.rs#L213), [configured chunk-index open](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/metadata/metadb/mod.rs#L812), [catalog path](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_object_locator/mod.rs#L195). The default chunk path is literally `.crab/chunk_index_db/`; scoped propagation is not inferred from the constant. |
| 7.2: catalog checkpoint, retirement, GC cadence | [publish_checkpoint](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_object_locator/writer.rs#L1301), [retire_old_catalog_checkpoints](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_object_locator/writer.rs#L1417), [locator_gc_due](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_object_locator/writer.rs#L1494), [open_for_catalog](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_object_locator/reader.rs#L145). Writer settings below `locator_gc_due` disable WAL/background GC; constants define 32-generation bands and two-hour retired checkpoint lifetime. |
| 7.2: pack-membership index | [writer keyspace/maintenance](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_object_locator/writer.rs#L1) and [reader OID/ordinal lookup](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_object_locator/reader.rs#L1). These are DB rows, not additional object-key families. |
| 8.1: ref/internal lease grammar and released payload | [path validation](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/push_lock.rs#L86), [expiry and clock probe](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/push_lock.rs#L744), [conditional tombstone release](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/push_lock.rs#L925). |
| 8.1: read/push admission and GC fences | [read resource prefix](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/read_admission.rs#L18), [push slots](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/push_admission.rs#L453), [push clock anchor](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/push_admission.rs#L417), [fence key](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-coordination/src/gc_fence.rs#L706). GC-fence payload is a different type from `PushLockPayload`. |
| 8.1, 9.1: native/LFS file locks | [unlock_with_id](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-lfs/src/lock.rs#L165) sets `released_at`; the same module defines `lfs/locks`, `locks/files`, and BLAKE3 of path bytes. |
| 8.2: GC run files and cleanup | [journal start/path helpers](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/journal.rs#L107), [complete/retire_artifacts](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/journal.rs#L659), [partition_for](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/marks.rs#L448). Completion retires batches/outcomes/marks before persisting the complete phase. |
| 9.1: LFS body and receipt fan-out | [object path](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-lfs/src/object_store.rs#L227), [receipt path and binding](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-lfs/src/object_store.rs#L841); receipt writer immediately above it uses best-effort overwrite. |
| 9.1: LFS cleanup boundary | [body delete](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-lfs/src/object_store.rs#L542), [lifecycle policy generation](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/lfs/lifecycle.rs#L127) and `list_lfs_objects` in the same file target body keys. Neither establishes receipt retirement. |
| 9.2: stage cache and experiments | [cache paths](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-workflow/src/cache.rs#L679), [publication order](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-workflow/src/cache.rs#L771), [experiment paths](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-workflow/src/experiment.rs#L114); named-output placement is resolved by `artifact_target` in the cache module. |
| 9.3: artifact paths, identity, and retained roots | [manifest path](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-workflow/src/artifact.rs#L1148), [identity and encoder](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-workflow/src/artifact.rs#L1936), [root visitor](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-workflow/src/artifact.rs#L779); payload, stage, pending, and history constructors are in the same module. Physical SDK conversion is a separate boundary. |
| 9.4: staging envelope and sessions | [session key constructors](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/receive/session.rs#L112), [receive validation](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/receive.rs#L2550), [TTL cleanup](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/receive/session.rs#L232), [helper cleanup calls](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/bin/crab_auth_receive.rs#L145). |
| 9.4: view placement | [snapshot caller](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/view.rs#L194) → [view key](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/view.rs#L763). The generation uses decimal display; the digest comes from the journal snapshot. |
| 10.3: repo GC candidate prefixes and roots | [candidate allowlist](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/mod.rs#L46), [in-memory metadata marks](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/mod.rs#L1681), [streaming metadata marks](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/mod.rs#L2176), [journal pack/edit roots](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/gc/mod.rs#L1934). No recursive whole-repository sweep is established by this allowlist. |
| 10.4: transitive GC gaps | Compare the two metadata visitors above with [graph layers](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/split_commit_graph.rs#L823), [pending recovery inputs](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/git_visibility.rs#L3281), and [mutable discovery](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/replication/discovery.rs#L22). The document reports missing proof in these visitors, not a reproduced deletion failure. |
| 11: measured inventory, sizes, headers, duplicate bytes | [Saved evidence](evidence/k8s-object-store-2026-09-03.json); recomputation and limitations below. |
| 12: existing geometric repack policy | [repack selection](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/repack.rs#L323) calls `crab_git::repack::geometric_repack_cut`; [stable-prefix test](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/repack.rs#L1676) specifies the intended preservation property. No new repack benchmark was run. |
| 13: mutation vocabulary | [Store::put](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-storage/src/store.rs#L405) is create-only with content matching; `create_strict`, `put_overwrite`, `update`, and `put_exact` follow in the same module. Lifecycle tables do not claim provider-level immutability. |
| 14–15: history prune versus collection | [prune_history](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/cmd/history_recovery.rs#L394) and [CLI reference](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/packages/web/content/docs/cli/reference/crab-recover.mdx). Default `apply = false` previews root pruning; downstream data collection is a separate operation. |

Two additional boundaries limit generalization of the core tables:

- [Database configuration overrides](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crab/src/core/config.rs#L2739)
  can replace the file/chunk DB paths. The default constants do not prove
  automatic scoped-root propagation for every caller.
- [Active-active receive finalization](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-auth-server/src/receive/finalize.rs#L56)
  commits coordinator refs before writing the regional
  [manifest projection](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/crates/crab-metadata/src/manifest_store.rs#L715).
  The direct journal/manifest publication description is not the authority
  contract for that external coordinator.

## Pinned dependency evidence

`Cargo.lock` selects [object_store 0.14.1](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/Cargo.lock#L6170) and
[slatedb 0.15.0](https://github.com/crabbuild/crab/blob/e26d139038414dcb8ddc591712d726f052547131/Cargo.lock#L8013). The installed package sources were
read directly; their file SHA-256 values are included under
`audited_dependency_sources` in the saved evidence.

| Dependency claim | Inspected source |
| --- | --- |
| `Path::from` encodes problematic segments and drops empty components | `object_store-0.14.1/src/path/mod.rs`: documented vectors at lines 118–145; `From<&str>` and `FromIterator` at lines 392–425; `src/path/parts.rs` defines the encoding set and segment conversion. |
| `Path::parse` differs from the encoding constructor | Same `mod.rs`, `Path::parse` at line 178: strips boundary slashes, validates segments, preserves the resulting string. This does not prove URL parsing or feature-name encoding is correct end to end. |
| SlateDB owns checkpoint/manifest/SST liveness | `slatedb-0.15.0/src/garbage_collector.rs`, collector tasks and expired-checkpoint filtering; `src/config.rs`, GC and reader options. Crab selects its settings in the catalog/file-index wrappers cited above. |

The [S3 key documentation](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html)
supports UTF-8, case sensitivity, flat keys, and the 1,024-byte limit.
[Git's pack specification](https://git-scm.com/docs/gitformat-pack) supports
the pack/index/reverse-index distinction. [SlateDB checkpoints](https://slatedb.io/docs/design/checkpoints/)
explain checkpoint liveness conceptually; exact behavior in this audit comes
from the pinned source, not an assumption that today's website matches every
dependency version.

The web fetcher could not retrieve the versioned docs.rs source pages during
this audit. They are not used as fetched evidence. Package source inspection
and the recorded hashes identify what was actually examined.

## Saved RustFS evidence

[evidence/k8s-object-store-2026-09-03.json](evidence/k8s-object-store-2026-09-03.json)
contains only object inventory fields, selected metadata, headers, and
verification results. Pusher and lock-holder identities are omitted.

The historical inventory has 235 rows. Its row columns are `Key`, `Size`,
`LastModified`, and `ETag`. Prefix grouping and integer summation reproduce
the table in section 11; the two root objects are grouped together there.
The 16 shared rows reproduce the separate 579,388,387-byte attribution.

The pack-segment bodies identify three distinct packs with 1,646,507, 4, and
6 objects. The current catalog checkpoint and selected visibility fields
record 1,646,517 objects at generation 3. This supports the numeric breakdown;
calling the additions specific user pushes or proving all objects' semantic
Git reachability would require more evidence than those counts alone.

For each of the 37 saved pack headers, decode the 12 recorded bytes: `PACK`,
big-endian version, and big-endian object count. Compare the counts to the
canonical pack entries or generated descriptors. Header checks are not full
pack integrity checks.

The two `original_stream_sha256_results` records have equal size
(1,267,048,888) and equal full-stream SHA-256. They are measurements retained
from the original inspection. Large pack bodies are not included, so a
reviewer cannot independently recompute those stream hashes from this JSON.
This audit did not re-download/re-hash both large objects. The generated copy
was absent from the follow-up listing.

The original verifier reports matching start/end key, size, ETag, and mtime
inventories. Only one of those historical listings is retained here; the
start/end comparison is a saved test result, not an independently repeatable
offline comparison. ETags are opaque version evidence, not assumed content
hashes.

The follow-up listing has 164 objects and 1,559,141,400 bytes. Comparing keys
shows 74 removals (68 generated-pack objects and six metadata objects) and
three additions under `gc/`. This proves a changed inventory, not which actor
made the changes, why they occurred, or that the resulting repository passes
integrity/recovery checks.

### Offline recomputation

Run from the repository root; no credentials or network access are needed:

```bash
python3 - <<'PY'
import collections
import json
from pathlib import Path

p = Path("crab/docs/design/evidence/k8s-object-store-2026-09-03.json")
d = json.loads(p.read_text())
rows = d["historical_repository_objects"]
groups = collections.defaultdict(lambda: [0, 0])
for key, size, modified, etag in rows:
    group = key.split("/")[1]
    groups[group][0] += 1
    groups[group][1] += size
print(dict(sorted(groups.items())))
assert len(rows) == 235
assert sum(row[1] for row in rows) == 3038625890
shared = d["attributed_shared_objects"]
assert len(shared) == 16
assert sum(row[1] for row in shared) == 579388387
proof = d["original_stream_sha256_results"]
assert proof[0]["bytes"] == proof[1]["bytes"] == 1267048888
assert proof[0]["sha256"] == proof[1]["sha256"]
for row in d["saved_pack_headers"]:
    header = bytes.fromhex(row["header_hex"])
    assert len(header) == 12 and header[:4] == b"PACK"
    assert int.from_bytes(header[8:12], "big") == row["object_count"]
live = d["audit_live_listing"]["objects"]
assert len(live) == 164
assert sum(row[1] for row in live) == 1559141400
assert not any("/generated-packs/" in row[0] for row in live)
print("Saved inventory arithmetic and header records agree.")
PY
```

## Search scope and remaining proof

The key audit searched production Rust source under `crab/src` and `crates`,
including explicit metadata/LFS/journal/view strings and owner path
constructors, then followed the noncanonical compactor result into CLI
dispatch and its read/CAS callees. Negative findings about publishers or
collectors are limited to the named revision and inspected paths. No
release-tag compatibility matrix was established.

The proposed byte-preserving join, canonical-pack descriptor reuse,
transitive GC fixes, journal/tombstone compaction, and view retirement still
require implementation decisions and E2E evidence. Source/format checks do
not close those gates.

**Is this PR the best fix for the documentation request?** Correcting the
inventory and its claims, preserving the distinction between source facts and
proposals, and attaching reproducible evidence addresses the requested audit.
Implementing the discovered runtime changes in this documentation PR would
need separate design and validation. This is not a verdict that the current
storage implementation satisfies the proposed standard.
