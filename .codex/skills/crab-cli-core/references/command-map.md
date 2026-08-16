# Crab CLI command map

Use this map to choose the narrowest skill. The command implementation in
`crab/src/main.rs` and the module under `crab/src/cmd/` remain authoritative
when this map and a guide disagree.

| Skill | Primary commands | Main source and guide surfaces |
| --- | --- | --- |
| `crab-repository-lifecycle` | `init`, `setup`, `clone`, `mirror`, `worktree`, `config`, `track`, `untrack`, `install`, `uninstall`, `completions` | `crab/src/cmd/{init,setup,clone,mirror,worktree,config,track}.rs`; guides `init`, `getting-started`, `clone`, `adopting-existing-repos`, `project-config`, `worktree` |
| `crab-large-files` | `add`, `reset`, default `status`, `why`, `hydrate`, `dehydrate`, `diff`, `diff-driver`, `ls-files`, hidden native `FilterProcess`, `fetch`, `prune`, `du`, `stat`, `cache`, `staging`, `adopt`, `unadopt`, `undo`, `migrate`, selective `download` | `crab/src/{engine,hydrate,cache,cmd/{add,reset,status,hydrate,dehydrate,diff,download,migrate,staging}}`; guides `add`, `hydrate`, `dehydrate`, `status`, `large-file-versioning`, `staging`, `cache`, `migrate` |
| `crab-git-sync` | `push`, `pull`, `ship`, `import`, `export`, `lock`, `unlock`, `locks`, Git remote-helper behavior | `crab/src/git/`, `crab/src/cmd/{push,pull,ship,import,export,lock}.rs`; design `push`; guides `fetch`, `import`, `export`, `ship`, `lock` |
| `crab-workflow` | `run`, `repro`, `stage`, `freeze`, `unfreeze`, `exp`, `queue`, `workflow`, `params`, `metrics`, `plots`, `status --workflow`, `migrate from-dvc` | `crab/src/workflow/`, `crab/src/cmd/{run,stage,freeze,exp,exp_queue,workflow,status_workflow,workflow_journal,workflow_lockfile,params,metrics}.rs`; guides `workflow`, `hermetic-workflows`, `project-config`; `crates/crab-workflow/` |
| `crab-lfs` | `lfs` and hidden `lfs-transfer-agent`; `optimize lfs` | `crab/src/lfs/`, `crab/src/cmd/lfs/`, `crates/crab-lfs/`; architecture `lfs-compatibility`; guide `lfs` |
| `crab-storage-ops` | `gc`, `fsck`, `compact`, `repack`, `optimize plan/apply/repo/xorbs/packs/shards/cache/indexes`, `metadb`, maintenance `cache` and `staging`, remote/index-focused `du` and `stat` | `crab/src/{metadata,storage,restripe,cmd/gc/,cmd/{fsck,compact,repack,optimize,metadb,staging,stat}}`; guides `gc`, `fsck`, `repack`, `optimize-xorbs`, `metadb`, `staging`, `du`, `stat` |
| `crab-tier-replication` | `tier`, `optimize tiers`, `replica`, `optimize replicas`, hidden `coordinator` | `crab/src/{tier,replication,restripe,cmd/tier/,cmd/{replica,coordinator}}`; guides `tier`, `replica`, `cost`; designs `replica-*`; `crates/crab-coordination/` |
| `crab-mount` | `mount`, `unmount`, `daemon`, hidden coordinator lifecycle | `crab/src/{vfs,git/worktree_hydration,cmd/mount}.rs`; architecture `virtual-filesystem`, `vfs-coordinator`, `nfs-mount-architecture`; guides `mount`, `daemon` |
| `crab-managed-operations` | `login`, `logout`, `auth`, `organization`, `repo`, `member`, `service-account`, `audit`, `release` | `crab/src/{auth,audit,release,cmd/{login,logout,auth_status,managed_admin,audit,release}}`; guides `auth/*`, `audit`, `release-manifests` |
| `crab-diagnostics-recovery` | `doctor`, `env`, `errors`, `logs`, `version`, `update`, `recover` and `recover history` | `crab/src/{core/error_catalog,cmd/{doctor,env,errors,logs,version,update,recover,history_recovery}}`; guides `doctor`, `env`, `errors`, `logs`, `recovery`, `repository-recovery` |
| `crab-cli-verification` | End-to-end proof for any command | `.codex/skills/crab-cli-verification/`; local RustFS fixture and side-effect proof |
| `crab-release-publish` | Publishing Crab CLI binaries and updating Homebrew | `.codex/skills/crab-release-publish/`; release scripts and GitHub release contract |

## Boundary rules

- A user-facing large-file operation belongs to `crab-large-files`; a remote
  ref/object transfer belongs to `crab-git-sync`.
- `du` and `stat` go to `crab-large-files` when the question is local file,
  staging, or hydration space; use `crab-storage-ops` when the question is
  remote object classes, indexes, or storage maintenance.
- `crab lfs` is always routed to `crab-lfs`, even when the request mentions
  large files. Native Crab pointer workflows and Git LFS compatibility are
  related but have different pointer formats and protocols.
- Workflow stage outputs belong to `crab-workflow`; repository-level file
  hydration of those outputs belongs to `crab-large-files`.
- `optimize` is routed by the noun after it: storage/index/cache work to
  `crab-storage-ops`, tier/replica work to `crab-tier-replication`, LFS work
  to `crab-lfs`, and workflow-cache work to `crab-workflow`.
- `crab release` creates dataset release manifests. Publishing Crab CLI
  archives is the separate `crab-release-publish` skill.
