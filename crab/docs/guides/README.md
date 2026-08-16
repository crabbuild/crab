# Crab Command Reference

Complete guide to every crab command. Each document covers synopsis, options,
examples, and related commands.

## Getting Started

| Command | Description |
|---------|-------------|
| [`crab init`](crab-init.md) | Initialize a new crab repository |
| [`crab clone`](crab-clone.md) | Clone a crab repository in one step |
| [`crab mirror`](mirror.md) | Mirror an external Git repository into a Crab remote |
| [`crab import`](crab-import.md) | Onboard an existing object-storage bucket as a Crab repo |
| [`crab export`](export.md) | Export a Crab snapshot as raw materialized files |
| [`crab install`](crab-install.md) | Install/uninstall crab git drivers |
| [`crab track`](crab-track.md) | Track/untrack file patterns |

## Daily Workflow

| Command | Description |
|---------|-------------|
| [`crab add`](crab-add.md) | Stage files (parallel, faster than git add) |
| [`crab reset`](crab-reset.md) | Unstage files and clean staging data |
| [`crab hydrate`](crab-hydrate.md) | Materialize pointer files into full content |
| [`crab dehydrate`](crab-dehydrate.md) | Replace files with pointers to free space |
| [`crab status`](crab-status.md) | Report hydration state of the working tree |
| [`crab diff`](crab-diff.md) | Chunk-level diff between git refs |
| [`crab fetch`](crab-fetch.md) | Pre-fetch objects into the local cache |
| [`crab worktree`](crab-worktree.md) | Manage Git worktrees with Crab hydration state |

## File Inspection

| Command | Description |
|---------|-------------|
| [`crab ls-files`](crab-ls-files.md) | List tracked files with hydration state |
| [`crab du`](crab-du.md) | Disk usage breakdown |
| [`crab stat`](crab-stat.md) | Staging area and performance statistics |

## Collaboration

| Command | Description |
|---------|-------------|
| [`crab lock`](crab-lock.md) | Advisory file locking (lock/unlock/locks) |

## Maintenance

| Command | Description |
|---------|-------------|
| [`crab gc`](crab-gc.md) | Garbage collect unreachable remote objects |
| [`crab fsck`](crab-fsck.md) | Check and repair repository integrity |
| [`crab repack`](crab-repack.md) | Consolidate remote Git pack files |
| [`crab optimize xorbs`](optimize-xorbs.md) | Rewrite xorbs to a workload-specific size profile |
| [`crab replica`](replica.md) | Configure primary-write read replicas |
| [`crab audit`](audit.md) | Inspect and verify local audit events |
| [`crab release`](release-manifests.md) | Dataset/model release manifest namespace |
| [`crab recover`](repository-recovery.md) | Operator recovery plan/apply namespace |
| [`crab prune`](crab-prune.md) | Remove unreferenced local cache objects |
| [`crab staging`](crab-staging.md) | Manage the local staging area |
| [`crab cache`](crab-cache.md) | Manage the local chunk cache |
| [`crab migrate`](crab-migrate.md) | Rewrite history for crab tracking |

## Virtual Filesystem

| Command | Description |
|---------|-------------|
| [`crab mount`](crab-mount.md) | Mount a virtual filesystem for on-demand access |
| [`crab daemon`](crab-daemon.md) | Multi-repo daemon with shared cache |

## Workflow

| Guide | Description |
|-------|-------------|
| [Workflow](crab-workflow.md) | Content-addressed caching and deterministic replay: `crab run`, `params`, `metrics`, `exp`, and the run journal |

## Guides

| Guide | Description |
|-------|-------------|
| [ETL and Data Platform](data-platform.md) | Pinned inputs, lineage manifests, catalog export, cache warming, and table coexistence |
| [Large-File Versioning](large-file-versioning.md) | Chunk/range diff, safetensors, Parquet, RAG, and model-evaluation evidence |
| [Native LFS Import](native-lfs-import.md) | Import LFS-format object-storage trees into Crab-native content |
| [Release Manifests](release-manifests.md) | Reproducible dataset/model release records |
| [Repository Recovery and Repair](repository-recovery.md) | Historical manifest restore plus operator recovery planning and verified repair |
| [Hermetic Workflows](hermetic-workflows.md) | Workflow sandbox enforcement for declared deps and outs |
| [Audit Logs](audit.md) | Local audit events, digest verification, export, and future mutation hooks |
| [Prefetch Profiles](crab-prefetch.md) | Always-materialized files, named hydration sets for CI/IDE/monorepo workflows |
| [Local Dev: RustFS](local-dev-rustfs.md) | Run a local S3-compatible backend for end-to-end testing |
| [Enterprise Auth](auth/enterprise-auth.md) | Federated identity and multi-cloud credential management |
| [Auth: Static / Multi-Cloud](auth/enterprise-auth-static.md) | Default env-var credentials for S3, GCS, or Azure |
| [Auth: AWS OIDC](auth/enterprise-auth-aws.md) | Corporate IdP → AWS STS temporary credentials |
| [Auth: GCP Workload Identity](auth/enterprise-auth-gcp.md) | Corporate IdP → GCP Workload Identity Federation |
| [Auth: Azure Entra ID](auth/enterprise-auth-azure.md) | Corporate IdP → Azure Blob Storage via Entra ID |
| [Auth: Crab Auth](auth/enterprise-auth-crab-auth.md) | Corporate IdP → custom authorization endpoint |
| [Structured Output](structured-output.md) | `--json` / `--jsonl` envelope contract, error schema, command table |

## Configuration & Diagnostics

| Command | Description |
|---------|-------------|
| [`crab config`](crab-config.md) | Read/write crab configuration |
| [`crab doctor`](crab-doctor.md) | Comprehensive health check |
| [`crab env`](crab-env.md) | Print diagnostic environment information |
| [`crab version`](crab-version.md) | Print version information |
| [`crab errors`](crab-errors.md) | Look up error codes |
| [`crab logs`](crab-logs.md) | Manage diagnostic log files |

## Git LFS Compatibility

| Command | Description |
|---------|-------------|
| [`crab lfs`](crab-lfs.md) | Full Git LFS-compatible command set |

## Internal (not for direct use)

| Command | Description |
|---------|-------------|
| [`crab filter-process`](crab-filter-process.md) | Git clean/smudge filter driver |
| [`crab diff-driver`](crab-diff-driver.md) | Git external diff driver |
| [`crab recovery`](crab-recovery.md) | Inflight operation recovery |
