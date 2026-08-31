# Crab X Launch Series — Baoyu Illustration Outline

## Direction

- Preset: `tech-explainer`
- Type: infographic
- Style: blueprint
- Language: English
- Aspect ratio: 16:9
- Count: 20 images, exactly two for each post
- Visual rule: every image explains a concrete Crab contract, flow, boundary, or verified value from the repository. No generic cloud, crab, or developer imagery.

## Post 1 — What Crab Is

1. `01-crab-direct-storage-architecture.png`
   - Position: after the opening definition of Crab.
   - Purpose: show the complete direct-storage path and the absence of a Crab data server.
   - Evidence represented: the CLI/filter/remote-helper entry points, Git-history lane, large-file lane, object storage, pointers, xorbs, and shards.
2. `02-crab-open-source-module-map.png`
   - Position: after the open-source architecture overview.
   - Purpose: introduce the executable boundaries and the shared Rust crates that own each major responsibility.

## Post 2 — Why Large Files Belong to Commit History

3. `03-one-commit-code-and-data.png`
   - Position: after the explanation of pointers in Git.
   - Purpose: show one commit selecting both ordinary Git content and an exact large-file identity.
4. `04-split-history-vs-exact-history.png`
   - Position: after the critique of separate data versioning.
   - Purpose: contrast ambiguous “latest data” coordination with commit-bound, byte-exact reconstruction.

## Post 3 — The Git Boundaries

5. `05-git-filter-and-remote-helper.png`
   - Position: after the clean/smudge and remote-helper explanation.
   - Purpose: show the two Git extension boundaries Crab uses without replacing Git.
6. `06-crab-ownership-layers.png`
   - Position: after the internal ownership discussion.
   - Purpose: distinguish the Git, pointer, reconstruction, storage, and publication contracts.

## Post 4 — Chunking, Deduplication, and Xorbs

7. `07-gearhash-content-defined-chunking.png`
   - Position: after the content-defined chunking rationale.
   - Purpose: show local boundary resynchronization after a small insertion and the verified chunk-size policy.
8. `08-continuity-aware-xorb-packing.png`
   - Position: after the xorb packing rationale.
   - Purpose: explain how Crab balances deduplication with efficient range reads using verified thresholds.

## Post 5 — Safe Push Publication

9. `09-immutable-first-visibility-last.png`
   - Position: after the push-stage description.
   - Purpose: show that immutable dependencies become durable before a ref is made visible.
10. `10-per-ref-lease-and-cas.png`
    - Position: after the concurrency explanation.
    - Purpose: show two writers racing on one ref and the lease/CAS rule that permits one winner.

## Post 6 — Lazy Materialization

11. `11-lazy-clone-selective-hydration.png`
    - Position: after the clone/hydrate user flow.
    - Purpose: show a pointer-first worktree and selective path hydration.
12. `12-verified-hydrate-and-dehydrate.png`
    - Position: after the correctness guarantees.
    - Purpose: show reconstruction, range coalescing, whole-file verification, atomic materialization, and the clean-file dehydrate gate.

## Post 7 — Storage Layout and Garbage Collection

13. `13-canonical-object-store-layout.png`
    - Position: after the storage namespace explanation.
    - Purpose: distinguish global content-addressed objects, the global shared index, and repo-local packs, indexes, publication, and coordination state.
14. `14-reachability-and-grace-gc.png`
    - Position: after the garbage-collection safety model.
    - Purpose: show roots, reachability, inventory subtraction, and the minimum grace rule.

## Post 8 — LFS and Mirror Mode

15. `15-crab-native-and-lfs-coexistence.png`
    - Position: after the LFS compatibility section.
    - Purpose: contrast Crab’s chunked BLAKE3-native objects with whole-file SHA-256 LFS objects in one repository.
16. `16-mirror-mode-publication-order.png`
    - Position: after the mirror-mode workflow.
    - Purpose: show Crab data publication before the forge push and make the two-remote transaction boundary explicit.

## Post 9 — Failure Semantics and Repair

17. `17-failure-stage-outcomes.png`
    - Position: after the failure-semantics argument.
    - Purpose: map each failure stage to the durable state users should observe.
18. `18-durable-truth-vs-acceleration.png`
    - Position: after the doctor/fsck and observability discussion.
    - Purpose: separate authoritative origin state from rebuildable caches, indexes, and visibility proofs.

## Post 10 — Open-Source Invitation

19. `19-crab-repository-map.png`
    - Position: after the repository tour.
    - Purpose: show the actual monorepo surfaces, language, license, and workspace size.
20. `20-contribution-evidence-loop.png`
    - Position: before the final contributor invitation.
    - Purpose: turn the project’s validation philosophy into a concrete contribution and qualification loop.
