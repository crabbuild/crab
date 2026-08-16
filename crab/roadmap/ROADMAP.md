# Crab Spec Execution Roadmap

_Updated: 2026-05-06_

## All Specs — Status Overview

Historical spec directories are listed below. "Done" means all
required (non-`*`) tasks are complete. "Remaining" describes what's left.
"🆕 Proposed" specs have requirements + design + tasks drafted but no
implementation started. Task counts come from manual audit of each
`tasks.md`.

| # | Spec | Status | Tasks (done / total, non-optional) | Remaining |
|---|------|--------|------------------------------------|-----------|
| 1 | `crab-platform` | ⚪ Superseded | 13 / 135 | Original monolithic v1 spec. Work executed through focused specs. Archive-candidate. |
| 2 | `crab-data-plane` | ✅ Done | 83 / 84 | Optional tests only. |
| 3 | `crab-staging-scale` | ✅ Done | 69 / 69 | — |
| 4 | `crab-transport` | ✅ Done | 65 / 65 | Optional tests only. |
| 5 | `crab-ops` | ✅ Done | 63 / 63 | Optional tests/benches. |
| 6 | `crab-lazy-checkout` | ✅ Done | 32 / 34 | Optional tests only. |
| 7 | `crab-transport-gaps` | ✅ Done | 33 / 33 | Optional tests only. |
| 8 | `crab-performance` | ✅ Done | 25 / 25 | Optional tests only. |
| 9 | `crab-xet-compat` | ✅ Done | 46 / 48 | 2 non-optional items + optional prop test. |
| 10 | `crab-lfs` | ✅ Done | 60 / 60 | Optional tests only. |
| 11 | `crab-chunk-diff` | ✅ Done | 29 / 29 | Optional tests only. |
| 12 | `crab-concurrent-push` | ✅ Done | 29 / 29 | Optional tests scattered. |
| 13 | `crab-fuse-mount` | 🟡 ~74% (required) | 55 / 57 | 2 required tasks remain (read-only FUSE mount checkpoint; full acceptance bar). 17 optional tests. |
| 14 | `crab-vfs-enhancements` | ✅ Done | 74 / 74 | Optional tests only. |
| 15 | `crab-cache-service` | 🟡 ~82% | 37 / 45 | 8 required tasks remain: optional tests for tasks 43–46, client config tests (48.2), CachingStore tests (49.3–49.4), deployment validation (51). |
| 16 | `crab-xorb-formation` | ✅ Done | 34 / 34 | Optional tests/benchmarks only. |
| 17 | `global-dedup` | ✅ Done | 79 / 81 | Optional property tests (21–22) and E2E tests remain. |
| 18 | `crab-push-hardening` | ✅ Done | 31 / 31 | Optional integration/property tests remain. |
| 19 | `crab-structured-output` | ✅ ~89% | 64 / 64 | Core envelope + error schema shipped. 8 remaining items are schema gen + extended docs. |
| 20 | `crab-hydrate-perf` | ✅ Done | 32 / 32 | Optional tests remain. |
| 21 | `crab-enterprise-auth` | ✅ ~97% | 36 / 37 | 1 required task + 5 optional items remain. |
| 22 | `crab-rbac` | 🔴 0% | 0 / 70 | All task groups pending. Unblocked by enterprise-auth. |
| 23 | `prolly-chunk-index` | ⚪ Superseded | 0 / 61 | Superseded by `prolly-rope-index`. Archive-candidate. |
| 24 | `crab-delta-reconstruction` | 🔴 0% | 0 / 34 | All task groups pending. Depends on prolly-rope-index. |
| 25 | `crab-p2p-cache` | 📋 Design only | — | Has requirements.md and design.md; no tasks.md. |
| 26 | `crab-data-registry` | 🆕 Proposed | 0 / 25 | `crab get` / `import` / `update` for cross-repo dataset reuse. |
| 27 | `crab-sdk` | ✅ Done | 39 / 39 | All 3 phases complete. Rust SDK + Python binding + streaming. |
| 28 | `crab-materialize-policy` | 🆕 Proposed | 0 / 28 | Pluggable hydrate strategies: `reflink` / `hardlink` / `symlink` / `copy`. |
| 29 | `crab-tree-add` | 🆕 Proposed | 0 / 18 | Per-directory digest + `.crabignore` for fast-path skip. |
| 30 | `crab-metrics-diff` | 🆕 Proposed | 0 / 17 | `crab metrics diff/show` + `crab plots diff` across refs. |
| 31 | `crab-bucket-import` | ✅ Done (V1 shipped) | 119 / 139 | Optional property test (8.9) and `--estimate` sampling (15.3) remain. |
| 32 | `crab-encryption` | 🆕 Proposed | 0 / 24 | Client-side encryption-at-rest. Enterprise differentiator. |
| 33 | `crab-audit` | 🆕 Proposed | 0 / 28 | Audit log stream, Sigstore/cosign signing, clean-filter secret scanning. |
| 34 | `crab-storage-economy` | 🆕 Proposed | 0 / 113 | Lifecycle tiering, restore-aware hydrate, `crab doctor --cost`, repack profiles. |
| 35 | `crab-disaster-recovery` | 🆕 Proposed | 0 / 27 | Cross-region mirror, PITR, `fsck --bucket`, soft-delete + sweep. |
| 36 | `crab-windows-parity` | 🆕 Proposed | 0 / 29 | Full Windows support: CI matrix, MSI installer, WinFsp mount. |
| 37 | `crab-ux-polish` | 🆕 Proposed | 0 / 27 | Error hints, shell completions, `crab undo`, interactive `crab init`. |
| 38 | `crab-migrations` | 🆕 Proposed | 0 / 33 | `migrate-from-lfs`, `migrate-from-dvc`, `crab-hf`, `crab-lfs-server`. |
| 39 | `crab-integrations` | 🆕 Proposed | 0 / 28 | GitHub Action, MCP server, VS Code extension, JetBrains plugin. |
| 40 | `crab-governance` | 🆕 Proposed | 0 / 35 | Retention, GDPR `forget`, lineage, SBOM, OTel + Prometheus, CI perf budgets. |
| 41 | `crab-hydrate-scale` | 🆕 Proposed | 0 / 20 | Sparse hydrate by manifest, prefetch profiles, speculative co-access hydration. |
| 42 | `crab-platform-plus` | 🆕 Proposed | 0 / 31 | Read-replica sidecar (Helm + Docker), cloud onboarding, browser viewer. |
| 43 | `crab-e2e-code-review` | ✅ Done | 85 / 85 | All phases complete. |
| 44 | `push-shard-reconstruction-missing-chunks` | ✅ Done | 25 / 26 | Optional extras remain. |
| 45 | `crab-push-flow-optimization` | ✅ ~82% | 23 / 28 | Remaining: clippy + verify-ml + RSS/latency/disk measurements (5.2–5.6). |
| 46 | `crab-push-perf-tier2` | ✅ ~90% | 36 / 39 | Remaining: clippy + docs update + bench measurements. |
| 47 | `crab-smart-http-parity` | ✅ ~70% | 18 / 29 | Phases 1–2 complete. Phase 3 (polish: option surface, concurrent tests, fault injection, edge cases) + Phase 4 (enterprise: signed push, policy engine, v2/SHA-256) remain. |
| 48 | `prolly-rope-index` | 🟡 Design done | 0 / 62 | Prototype removed from `crab`; no crab-side integration landed. |
| 49 | `crab-workflow-gaps` | ✅ Done | 10 / 11 | 1 optional test pending. |
| 50 | `crab-workflow-layer` | 🟡 ~75% | ~65 / 90 | Phases 0–3 done. Phase 4 (experiments) ~80% done. Phase 5 (hermeticity/parallelism) + docs pending (~20 tasks). |
| 51 | `crab-marketing-site` | ✅ Done | 118 / 118 | All 14 phases shipped. |
| 52 | `crab-gitoxide-adoption` | ✅ ~85% | ~35 / 42 | Phase 0 + Phase 1 complete. Phase 2 (simplification) ~50% done. Phase 3 (opportunistic) pending. Sub-items within completed top-level tasks still open. |
| 53 | `crab-incremental-tree-diff` | 🆕 Proposed | 0 / 25 | O(changed files) pointer discovery. Builds on push-perf-tier2 C1. |
| 54 | `crab-unified-push-manifest` | 🆕 Proposed | 0 / 48 | Single-object CAS commit point. Eliminates split-brain window. |
| 55 | `crab-slatedb-metadata` | ✅ ~85% | ~42 / 50 | Core implementation done (scaffolding, stores, legacy deletion, push/hydrate/GC integration, observability). Optional E2E and property tests remain (~8 items marked `*`). |
| 56 | `crab-workflow-enrichment` | ✅ Done | 46 / 46 | All 4 phases complete: templating, foreach/matrix, experiments, polish. |
| 57 | `crab-workflow-engine` | ✅ Done | ~22 / 22 | All phases complete (A–D + verification). |
| 58 | `crab-desktop-pro` | ✅ ~80% | ~130 / 165 | Phases 1–4 complete (write-side loop, forge integration, ML/data workflow, worktrees). Phase 5 (Crab superpowers + security) + Phase 6 (team primitives + offline) pending. |
| 59 | `crab-desktop-flow` | ✅ Done | 60 / 60 | All 4 phases complete: structured add, visual push, credential config, polish. |
| 60 | `crab-desktop-ai-agent` | 🆕 Proposed | 0 / ~130 | 4-phase AI assistant: SDK integration + read tools, propose/approve tools, Tier 3 tools + progress, intelligence + persistence + cost. |
| 61 | `crab-auth-provisioning` | 🆕 Proposed | 0 / ~65 | Admin CLI wizard, credential vending service, remote config, audit logging. |
| 62 | `gcs-sdk-1x-migration` | 📋 Requirements only | — | Has requirements.md only; no design or tasks. |

---

## Correction Log (2026-05-06)

Discrepancies found during full re-audit of every `tasks.md`:

1. **`crab-sdk`** was listed as 🆕 Proposed (0/26). Actual: All 39 tasks
   across 3 phases are `[x]`. The Rust SDK, Python binding, and streaming
   `Reader`/`Stream` are all shipped. Moved to ✅ Done.

2. **`crab-smart-http-parity`** was listed as 🔴 0% (0/88). Actual: 18
   top-level tasks complete across Phases 1–2. The original "88 tasks" count
   included sub-items; the correct top-level count is 29. Phases 1–2
   (P0 correctness + protocol conformance core) are fully done. Revised to
   ~70%.

3. **`crab-gitoxide-adoption`** was listed as 🆕 Proposed (0/98). Actual:
   Phase 0 (6 top-level items) + Phase 1 (5 top-level items) all `[x]`.
   Phase 2 partially done (~4 of 11 items). The "98" count included all
   sub-items; top-level task count is ~42. Revised to ~85%.

4. **`crab-workflow-layer`** was listed as ~49% (44/90). Actual: Phases
   0–3 complete, Phase 4 (experiments) ~80% done (4.7 in-progress, 4.8–4.9
   pending). Phase 5 + docs (~20 items) pending. Revised to ~75%.

5. **Eight specs missing from the roadmap** added:
   - `crab-slatedb-metadata` (#55) — ✅ ~85%, core metadata layer shipped
   - `crab-workflow-enrichment` (#56) — ✅ Done, templating + experiments
   - `crab-workflow-engine` (#57) — ✅ Done, parallel scheduler + remote cache
   - `crab-desktop-pro` (#58) — ✅ ~80%, Phases 1–4 shipped
   - `crab-desktop-flow` (#59) — ✅ Done, structured add/push flows
   - `crab-desktop-ai-agent` (#60) — 🆕 Proposed, AI assistant
   - `crab-auth-provisioning` (#61) — 🆕 Proposed, admin CLI + CVS
   - `gcs-sdk-1x-migration` (#62) — 📋 Requirements only

6. **`crab-slatedb-metadata`** is a major architectural change (replaces
   legacy file-index with SlateDB) that was not in the roadmap despite being
   ~85% complete. It's a prerequisite for several performance and correctness
   improvements.

---

## Execution Order — Prioritized by Delivery Impact

Specs are grouped into waves based on dependencies, priority, and risk.
Within each wave, specs can be worked in parallel unless noted.

### Wave 0 — Close Out In-Progress Work (highest priority)

Reduce WIP by finishing specs that are 85%+ done.

| Priority | Spec | Effort | Why |
|----------|------|--------|-----|
| P0-1 | `crab-slatedb-metadata` — optional tests | ~2 days | Core metadata layer; optional E2E/property tests close it out. |
| P0-2 | `crab-gitoxide-adoption` — Phase 2 remainder | ~5 days | LOC reduction + correctness. Phase 2 items 7–11 remain. |
| P0-3 | `crab-smart-http-parity` — Phase 3 polish | ~5 days | Concurrent-push tests, fault injection, option surface. |
| P0-4 | `crab-workflow-layer` — Phase 4 finish + Phase 5 | ~10 days | Experiments + hermeticity + docs. |
| P0-5 | `crab-push-flow-optimization` — verify pass | ~1 day | Measurements + docs only. |
| P0-6 | `crab-push-perf-tier2` — verify pass | ~1 day | Bench numbers + docs update. |
| P0-7 | `crab-fuse-mount` — 2 required tasks | ~3 days | Read-only mount checkpoint + acceptance bar. |
| P0-8 | `crab-cache-service` — tests + deployment | ~4 days | 8 remaining required tasks. |
| P0-9 | `crab-desktop-pro` — Phase 5 (superpowers) | ~8 days | Chunk-aware blame, pointer lineage, storage explorer, secret scanning. |

**Total Wave 0 effort:** ~39 days

---

### Wave 1 — Enterprise & Protocol Completion

| Priority | Spec | Effort | Why |
|----------|------|--------|-----|
| P1-1 | `crab-smart-http-parity` — Phase 4 (enterprise) | ~10 days | Signed push, policy engine, v2/SHA-256. |
| P1-2 | `crab-rbac` | ~20 days | Completes enterprise auth story. Unblocked. |
| P1-3 | `crab-auth-provisioning` | ~20 days | Admin CLI + credential vending service. Pairs with RBAC. |

**Total Wave 1 effort:** ~50 days

---

### Wave 2 — Desktop AI + Architectural Bets

| Priority | Spec | Effort | Why |
|----------|------|--------|-----|
| P2-1 | `crab-desktop-ai-agent` | ~20 days | AI assistant for desktop app. High user-value differentiator. |
| P2-2 | `prolly-rope-index` | ~22 days | Foundation for delta-reconstruction + byte-range FUSE. |
| P2-3 | `crab-delta-reconstruction` | ~15 days | Key ML differentiator. Depends on prolly-rope-index. |
| P2-4 | `crab-desktop-pro` — Phase 6 (team + offline) | ~12 days | Workspaces, cloud patches, offline mode. |

**Total Wave 2 effort:** ~69 days

---

### Wave 3 — Ecosystem Growth (DVC-inspired, independently shippable)

| Priority | Spec | Effort | Why |
|----------|------|--------|-----|
| P3-1 | `crab-migrations` | ~22 days | Largest acquisition funnel. LFS/DVC/HF users. |
| P3-2 | `crab-data-registry` | ~6 days | Cross-repo dataset reuse. SDK is done (unblocked). |
| P3-3 | `crab-materialize-policy` | ~7 days | 10 GB hydrate in <1s on modern FS. |
| P3-4 | `crab-tree-add` | ~5 days | Fast-path skip for unchanged files. |
| P3-5 | `crab-metrics-diff` | ~4 days | Thin CLI over params/metrics. |
| P3-6 | `crab-incremental-tree-diff` | ~5 days | O(changed files) pointer discovery. |
| P3-7 | `crab-unified-push-manifest` | ~10 days | Eliminates split-brain window. |

**Total Wave 3 effort:** ~59 days

---

### Wave 4 — Security & Governance

| Priority | Spec | Effort | Why |
|----------|------|--------|-----|
| P4-1 | `crab-ux-polish` | ~10 days | Phase 1 alone is ~2 days for huge UX uplift. |
| P4-2 | `crab-encryption` | ~15 days | #1 enterprise differentiator. No competitor has this. |
| P4-3 | `crab-audit` | ~18 days | SOC 2 / FedRAMP requirement. |
| P4-4 | `crab-integrations` | ~14 days | GH Action, MCP, VS Code, JetBrains. SDK done (unblocked). |
| P4-5 | `crab-storage-economy` | ~12 days | Lifecycle tiering, cost visibility. |
| P4-6 | `crab-disaster-recovery` | ~15 days | Cross-region mirror, PITR. |
| P4-7 | `crab-windows-parity` | ~18 days | Full Windows support. |
| P4-8 | `crab-governance` | ~16 days | Retention, erasure, lineage, SBOM. |
| P4-9 | `crab-hydrate-scale` | ~9 days | Sparse manifest + prefetch. |
| P4-10 | `crab-platform-plus` | ~18 days | Replica sidecar, cloud onboarding, viewer. |

**Total Wave 4 effort:** ~145 days

---

### Wave 5 — Future / Needs Planning

| Spec | Status | Action |
|------|--------|--------|
| `crab-p2p-cache` | 📋 Design only | Write tasks.md before scheduling. |
| `crab-platform` | ⚪ Superseded | Archive. |
| `prolly-chunk-index` | ⚪ Superseded | Archive (superseded by prolly-rope-index). |
| `gcs-sdk-1x-migration` | 📋 Requirements only | Needs design + tasks. |

---

## Dependency Graph

```
Wave 0 (close WIP — all in-flight)
├── crab-slatedb-metadata  (optional tests)
├── crab-gitoxide-adoption  (Phase 2 remainder)
├── crab-smart-http-parity  (Phase 3 polish)
├── crab-workflow-layer  (Phase 4–5 + docs)
├── crab-push-flow-optimization  (verify pass)
├── crab-push-perf-tier2  (verify pass)
├── crab-fuse-mount  (2 required tasks)
├── crab-cache-service  (8 remaining)
└── crab-desktop-pro  (Phase 5)

Wave 1 (enterprise + protocol)
├── crab-smart-http-parity Phase 4 ← gitoxide-adoption Phase 1 ✅
├── crab-rbac ← crab-enterprise-auth ✅
└── crab-auth-provisioning ← crab-enterprise-auth ✅

Wave 2 (desktop AI + architectural bets)
├── crab-desktop-ai-agent  (independent)
├── prolly-rope-index ← design only
├── crab-delta-reconstruction ← prolly-rope-index
└── crab-desktop-pro Phase 6  (independent)

Wave 3 (ecosystem growth — all independent)
├── crab-migrations ← crab-sdk ✅
├── crab-data-registry ← crab-sdk ✅
├── crab-materialize-policy  (independent)
├── crab-tree-add  (independent)
├── crab-metrics-diff ← crab-structured-output ✅
├── crab-incremental-tree-diff ← push-perf-tier2 C1
└── crab-unified-push-manifest ← smart-http-parity P0-2 ✅

Wave 4 (security + governance + ecosystem reach)
├── crab-ux-polish  (independent)
├── crab-encryption  (independent)
├── crab-audit  (pairs with smart-http-parity Phase 4)
├── crab-integrations ← crab-sdk ✅
├── crab-storage-economy  (independent)
├── crab-disaster-recovery ← crab-audit
├── crab-windows-parity  (independent)
├── crab-governance ← crab-workflow-layer, crab-audit
├── crab-hydrate-scale ← crab-hydrate-perf ✅
└── crab-platform-plus ← crab-cache-service

Wave 5 (future)
├── crab-p2p-cache  (needs tasks.md)
├── crab-platform  (archive)
├── prolly-chunk-index  (archive)
└── gcs-sdk-1x-migration  (needs design)
```

---

## Estimated Timeline

| Wave | Specs | Effort | Calendar (1 eng) |
|------|-------|--------|------------------|
| 0 | Close WIP (9 specs) | ~39 days | ~8 weeks |
| 1 | Enterprise + protocol (3 specs) | ~50 days | ~10 weeks |
| 2 | Desktop AI + architectural (4 specs) | ~69 days | ~14 weeks |
| 3 | Ecosystem growth (7 specs) | ~59 days | ~12 weeks |
| 4 | Security + governance (10 specs) | ~145 days | ~29 weeks |
| 5 | Future / planning | TBD | TBD |
| **Total (Waves 0–4)** | | **~362 days** | **~73 weeks** |

With 2–3 engineers in parallel and the independent nature of Waves 3/4,
the critical path is significantly shorter:

- **Critical path (serial):** Wave 0 → Wave 1 RBAC → Wave 2 prolly-rope →
  delta-reconstruction = ~39 + 20 + 22 + 15 = **~96 days (~19 weeks)**
- **Parallel tracks:** Desktop AI, migrations, encryption, UX polish can
  all run concurrently with the critical path.

### Recommended Immediate Priorities (next 2 weeks)

1. **Close `crab-slatedb-metadata`** — optional tests only (~2 days)
2. **Close `crab-push-flow-optimization` + `crab-push-perf-tier2`** — verify passes (~2 days)
3. **`crab-workflow-layer` Phase 4 finish** — experiments completion (~3 days)
4. **`crab-smart-http-parity` Phase 3** — concurrent-push tests + fault injection (~5 days)

### Recommended Top-3 from Wave 4

1. **`crab-ux-polish`** — Phase 1 alone is ~2 days for hints across every error.
2. **`crab-encryption`** — #1 enterprise differentiator; no competitor has this.
3. **`crab-migrations`** — largest acquisition funnel (LFS/DVC/HF users).

---

## Key Risks

1. **Prolly rope migration** (`prolly-rope-index` SQLite → rope) —
   compatibility-mode detection and fallback must be bulletproof.
   One-way door per staging area.

2. **Smart-HTTP parity Phase 4** (signed push, policy engine) — large
   surface area touching the hot push path. Needs thorough concurrent-push
   integration tests (Phase 3 delivers these).

3. **`crab-rbac` scope** — 70 tasks including reference deployments and
   admin web UI. Consider phasing: core RBAC first, deployments and UI later.

4. **`crab-desktop-ai-agent`** — 130 tasks across 4 phases. Large scope.
   Phase 1 (read-only tools + streaming) is independently shippable and
   highest value.

5. **`crab-auth-provisioning`** — new binary (`crab-cvs`). Deployment
   complexity. Consider shipping admin CLI wizard (Phase 1) independently
   of the credential vending service.

6. **`crab-workflow-layer` Phase 5** — hermeticity via FUSE sandbox and
   parallel DAG execution are complex. Consider shipping Phase 4
   (experiments) as the v1 milestone and deferring Phase 5.

7. **`crab-platform` and `prolly-chunk-index` archival confusion** —
   both have `[ ]` tasks but are superseded. Archive to prevent
   double-counting.

8. **`crab-p2p-cache`** — design only; needs tasks.md before scheduling.

9. **Desktop Pro Phase 5–6** — 35+ tasks remaining. Phase 5 (superpowers)
   is the differentiator; Phase 6 (team/offline) can be deferred.

10. **Wave 3 `crab-migrations`** — now unblocked by SDK completion.
    Should be prioritized for user acquisition.

---

## Completed Specs Summary (for reference)

The following specs are fully done and require no further work:

- `crab-data-plane`, `crab-staging-scale`, `crab-transport`,
  `crab-ops`, `crab-lazy-checkout`, `crab-transport-gaps`,
  `crab-performance`, `crab-xet-compat`, `crab-lfs`,
  `crab-chunk-diff`, `crab-concurrent-push`, `crab-vfs-enhancements`,
  `crab-xorb-formation`, `global-dedup`, `crab-push-hardening`,
  `crab-hydrate-perf`, `crab-bucket-import`, `crab-e2e-code-review`,
  `push-shard-reconstruction-missing-chunks`, `crab-workflow-gaps`,
  `crab-marketing-site`, `crab-sdk`, `crab-workflow-enrichment`,
  `crab-workflow-engine`, `crab-desktop-flow`

**25 specs fully complete.** 12 specs in-flight. 25 specs proposed/pending.
