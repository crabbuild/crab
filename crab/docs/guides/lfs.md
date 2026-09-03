# crab lfs

Git LFS compatibility commands.

## Synopsis

```
crab lfs <subcommand> [OPTIONS]
```

## Description

`crab lfs` provides Crab-managed LFS operations against cloud object storage
without a centralized LFS server. The CLI supports the repository-scoped
standalone transfer-agent profile, which requires Git LFS to be configured to
invoke Crab and requires Crab to have access to the selected object store.
Crab does not provide or require a deployable Git LFS HTTP gateway. An
unmodified `git-lfs` installation can still use Crab's standalone custom
transfer agent: `crab lfs install` configures Git LFS to invoke the local Crab
binary, and Crab accesses the configured object store directly.

The core transfer flow mirrors the supported standalone Git LFS profile:
clean/smudge, custom transfer-agent,
push, fetch, pull, checkout, pointer inspection, status, local fsck, local
pruning, direct LFS/Crab conversion, and safe local deduplication.

Crab honors the standard `lfs.storage` local cache control consistently across
filters and LFS commands. Relative paths are resolved from the repository's
common Git directory, so linked worktrees share the same cache. Crab also
accepts the legacy `lfs.lfsdir` and `GIT_LFS_DIR` aliases; use `lfs.storage`
when sharing a cache with an unmodified Git LFS client.

Remote setup for push, object-ID push, pre-push, fetch and pull uses the
command's cancellation token. Named-remote Git lookups stop and join their
owned subprocess on cancellation; cancellation is not reported as a missing
remote or reinterpreted as a revision. Captured lookup stdout and stderr are
bounded independently at 64 MiB. Pending client setup also stops before
transfer discovery when cancelled. Conversion and remote-verified pruning
pass their existing cancellation token through the same setup owner.

This is not whole-command cancellation coverage: idle stdin, identity and
other Git queries, transfer-agent sessions, and some transfer/verification
awaits still require separate lifecycle work. Commands without a caller token
continue to use the existing public setup entry points.

## Subcommands Overview

| Command | Description |
|---------|-------------|
| `install` | Configure git to use crab as the LFS transfer agent |
| `uninstall` | Remove crab LFS transfer agent configuration |
| `update` | Update git hooks and filter configuration for LFS |
| `clone` | Deprecated Git LFS clone compatibility wrapper |
| `completion` | Generate shell completion scripts |
| `ext` | View configured Git LFS extension details |
| `track` | Track files matching a pattern with LFS |
| `untrack` | Stop tracking files matching a pattern with LFS |
| `fetch` | Download LFS objects from the remote store |
| `pull` | Fetch LFS objects and replace pointers in the working tree |
| `push` | Upload LFS objects to the remote store |
| `pre-push` | Pre-push hook: upload missing LFS objects before push completes |
| `post-checkout` | Post-checkout hook: update write bits for lockable LFS files |
| `post-commit` | Post-commit hook: update write bits for lockable LFS files |
| `post-merge` | Post-merge hook: update write bits for lockable LFS files |
| `checkout` | Replace LFS pointers in the working tree with actual content |
| `lock` | Create an advisory lock on an LFS-tracked file |
| `unlock` | Remove an advisory lock from an LFS-tracked file |
| `locks` | List all active LFS file locks |
| `ls-files` | List LFS-tracked files and their status |
| `status` | Show staged and modified LFS-tracked files |
| `fsck` | Verify integrity of local LFS objects |
| `prune` | Remove unreferenced LFS objects from local storage |
| `convert` | Convert files between LFS and crab-native pointer formats |
| `migrate` | Rewrite history to convert files to/from LFS pointers |
| `pointer` | Generate, validate, or inspect LFS pointers |
| `clean` | Standalone clean filter (stdin → stdout) |
| `smudge` | Standalone smudge filter (stdin → stdout) |
| `filter-process` | Long-running clean/smudge filter process |
| `merge-driver` | Git LFS merge driver endpoint for text LFS files |
| `standalone-file` | Internal JSON transfer adapter endpoint |
| `env` | Print LFS diagnostic environment information |
| `version` | Print crab LFS version information |
| `dedup` | Deduplicate checked-out LFS files with the local LFS object store |
| `logs` | Display Git LFS-style error logs |

---

## crab lfs install

Configure git to use crab for LFS filters and transfers.

```bash
crab lfs install [--local|--worktree|--system] [--force] [--manual] [--skip-smudge] [--skip-repo]
```

| Option | Description |
|--------|-------------|
| `--local` | Write configuration to the local `.git/config` only |
| `--worktree` | Write configuration to the current worktree config |
| `--system` | Write configuration to the system Git config |
| `--force` | Overwrite an existing pre-push hook |
| `--manual` | Print commands instead of modifying config or hooks |
| `--skip-smudge` | Set the smudge filter to skip mode (pointers not expanded on checkout) |
| `--skip-repo` | Skip installing the repository pre-push hook |

### Example

```bash
crab lfs install
crab lfs install --local --skip-smudge  # CI-friendly setup
```

The command configures Git to use Crab's standalone clean/smudge filters for
LFS pointers and configures `git-lfs` to invoke Crab's standalone
transfer-agent entry point:

```ini
[filter "lfs"]
    clean = /path/to/crab lfs clean
    smudge = /path/to/crab lfs smudge
    required = true
[lfs "customtransfer.crab"]
    path = /path/to/crab
    args = lfs-transfer-agent
    concurrent = true
    direction = both
[lfs]
    standalonetransferagent = crab
```

With `--skip-smudge`, `filter.lfs.smudge` is set to
`/path/to/crab lfs smudge --skip -- %f`, so checkout leaves pointers in place
until a later `crab lfs pull` or `crab lfs checkout`.

---

## crab lfs uninstall

Remove crab LFS filter and transfer configuration.

```bash
crab lfs uninstall [--local|--worktree|--system] [--skip-repo]
```

| Option | Description |
|--------|-------------|
| `--local` | Remove configuration from the local `.git/config` only |
| `--worktree` | Remove configuration from the current worktree config |
| `--system` | Remove configuration from the system Git config |
| `--skip-repo` | Skip removing Crab's repository pre-push hook |

Without a scope flag, install and uninstall operate on the current
repository's local Git config. Use `--system` only when intentionally managing
an explicit system-wide installation; it can affect unrelated repositories.

---

## crab lfs update

Update git hooks and filter configuration for LFS.

```bash
crab lfs update [--force] [--manual]
```

| Option | Description |
|--------|-------------|
| `--force` | Overwrite existing hooks even if modified |
| `--manual` | Display commands instead of modifying configuration |

---

## crab lfs clone

Deprecated Git LFS clone compatibility wrapper.

```bash
crab lfs clone [GIT_CLONE_OPTIONS] <repository> [directory]
```

Crab disables LFS clean/smudge filters during the underlying `git clone`, then
runs `crab lfs pull` from inside the cloned repository. With `--no-checkout`,
`-n`, `--bare`, or `--mirror`, it runs `crab lfs fetch` instead. `--include`,
`-I`, `--exclude`, and `-X` apply to the post-clone LFS transfer. `--skip-repo`
skips installing local LFS hook and filter configuration.

This command uses Crab's object-store LFS backend. It is not a generic Git LFS
HTTP clone replacement.

---

## crab lfs completion

Generate a shell completion script for Crab commands, including `crab lfs`.

```bash
crab lfs completion <bash|zsh|fish|powershell>
```

The generated script is printed to stdout.

---

## crab lfs ext

View configured Git LFS extension details.

```bash
crab lfs ext
crab lfs ext list [NAME...]
```

Crab reads `lfs.extension.<name>.clean`, `.smudge`, and `.priority` from Git
configuration and prints the same fields as Git LFS. Configured extensions run
in the clean pipeline in ascending priority order, and pointer extension
metadata runs the matching smudge commands in reverse order.

---

## crab lfs track / untrack

Track or untrack files with LFS.

```bash
crab lfs track [OPTIONS] [pattern...]  # Track patterns, or list tracked patterns
crab lfs untrack <pattern...>  # Stop tracking one or more patterns
```

| Option | Description |
|--------|-------------|
| `--filename` | Treat arguments as literal filenames and escape glob characters |
| `--lockable`, `-l` | Add the `lockable` attribute so files should be locked before editing |
| `--not-lockable` | Remove the `lockable` attribute from matching LFS tracking entries |
| `--no-excluded` | List only LFS-tracked patterns |
| `--force` | Replace an existing Crab/XET tracking entry for the same pattern |
| `--dry-run`, `-d` | Preview `.gitattributes` changes without writing |
| `--verbose`, `-v` | Print files checked for existing Git index matches |
| `--no-modify-attrs` | Mark matching tracked files stat-dirty without editing `.gitattributes` |

### Examples

```bash
crab lfs track '*.bin'       # Track all .bin files
crab lfs track --lockable '*.psd'
crab lfs track               # List currently tracked patterns
crab lfs untrack '*.bin' '*.psd'
```

Output when listing:

```
    *.bin (filter=lfs diff=lfs merge=lfs -text)
    *.safetensors (filter=lfs diff=lfs merge=lfs -text)
```

---

## crab lfs fetch

Download LFS objects from the remote store.

```bash
crab lfs fetch [OPTIONS] [REMOTE] [REF...]
```

| Option | Description |
|--------|-------------|
| `REMOTE` | Git remote name whose URL points at a Crab remote; omitted uses Crab's configured default |
| `REF...` | Fetch objects referenced by these refs |
| `--include`, `-I` | Include only paths matching this pattern |
| `--exclude`, `-X` | Exclude paths matching this pattern |
| `--recent`, `-r` | Fetch objects for recent refs and recent commits using Git LFS recent config |
| `--all`, `-a` | Fetch all LFS objects ever referenced by local refs or the provided refs |
| `--stdin` | Read refs from stdin |
| `--prune`, `-p` | Run local LFS prune after a successful fetch |
| `--refetch` | Download matching objects even when they already exist locally |
| `--dry-run`, `-d` | Report what would be fetched without downloading |
| `--json`, `-j` | Output a stable JSON transfer list |

`--json` cannot be combined with `--prune`, matching Git LFS. JSON transfer
actions describe Crab's object-store path instead of a Git LFS HTTP endpoint.
`--recent` includes the requested refs plus refs selected by
`lfs.fetchrecentrefsdays` / `lfs.fetchrecentremoterefs` and commits selected by
`lfs.fetchrecentcommitsdays`.

With `--stdin`, refs come only from the input stream; command-line refs cannot
be mixed with it. Empty input selects no refs unless `--all` or `--recent`
explicitly requests broader selection. An empty successful `--json` fetch
returns an empty transfer list. Explicit `--prune` still runs after an empty
fetch. Input framing and limits are shared with [push](#crab-lfs-push).

Git subprocesses used for recent-ref selection honor command cancellation and
cap captured stdout and stderr independently at 64 MiB. Oversized output is
an error, not a truncated selection. This bound does not cover total command
memory or imply that every prune traversal is cancellable.

### Example

```bash
crab lfs fetch --include '*.safetensors'
crab lfs fetch --all
crab lfs fetch --dry-run
crab lfs fetch --refetch --json origin main
```

---

## crab lfs pull

Fetch LFS objects and replace pointers in the working tree.

```bash
crab lfs pull [--include <PATTERN>] [--exclude <PATTERN>] [REMOTE]
```

`REMOTE` selects a Git remote whose URL points at a Crab remote. This is
equivalent to `crab lfs fetch [REMOTE]` followed by `crab lfs checkout`, so
checkout updates only missing files or matching LFS pointer placeholders from
the local LFS cache.

---

## crab lfs push

Upload LFS objects to the remote store.

```bash
crab lfs push [OPTIONS] [REMOTE] [REF...]
```

| Option | Description |
|--------|-------------|
| `REMOTE` | Git remote name or Crab remote URL; omitted uses Crab's configured default |
| `REF...` | Select reachable history; omitted defaults to `HEAD` without `--all` or `--stdin` |
| `--all`, `-a` | Scan complete selected history; omitted refs select local branches and tags |
| `--object-id`, `-o` | Upload specific LFS object IDs |
| `--stdin` | Read refs or object IDs from stdin |
| `--dry-run`, `-d` | Report what would be pushed without uploading |

Ordinary push includes historical versions replaced or deleted before the
selected tip, excluding objects reachable from the selected named remote's
local tracking refs. Other remotes do not narrow the upload. A direct URL or
Crab's configured default has no named tracking set, so no tracking-history
exclusion is applied. Omitted refs defaulting to `HEAD` is a Crab extension;
[Git LFS 3.8](https://github.com/git-lfs/git-lfs/blob/v3.8.0/commands/command_push.go)
requires explicit refs for ordinary non-stdin push.

Use `--all` to scan complete selected history without remote-tracking
exclusions, including when restoring missing remote payloads. Its omitted-ref
scope excludes remote-only refs and detached history; select those explicitly.
This differs from `crab lfs fetch --all`, whose omitted-ref scope is all refs.
Both upload modes inspect local Git objects only: missing promised blobs,
corrupt objects or conflicting pointer sizes fail instead of returning a
partial inventory or implicitly fetching source history.

With `--object-id`, every operand must be a 64-character hexadecimal SHA-256
object ID. Invalid trailing IDs are errors, not silently skipped. For actual
uploads, Crab reads sizes from the standard local LFS cache and rejects missing
files before transferring payloads. Object-ID `--dry-run` validates syntax but
does not yet check local payload availability.

`--stdin` accepts one ref or object ID per line, including CRLF and a final
line without a newline. Empty lines are ignored; other whitespace is preserved.
Invalid UTF-8 and control bytes are errors. Command-line refs/IDs cannot be
mixed with stdin; incompatible flags are rejected before waiting for input.
Empty input is a successful no-op, except `--all` still selects local branches
and tags. It never implicitly changes object-ID mode into a `HEAD` upload.

Push and fetch share fixed admission limits: 1 MiB per encoded line, 64 MiB
total encoded input, and a separate 64 MiB logical operand-inventory budget
(string descriptors plus operand bytes). Line terminators count toward encoded
limits. Exceeding a limit rejects the whole request; no partial operand list
is transferred. These are admission bounds, not a total process RSS guarantee.
Cancellation is checked between reads; a producer holding an incomplete line
open can still block the reader. Interruptible idle input remains unqualified.

---

## crab lfs pre-push

Pre-push hook handler. Invoked automatically by git's pre-push hook to upload
missing LFS objects before the push completes. Not intended for direct user
invocation.

```bash
crab lfs pre-push
```

---

## crab lfs post-checkout / post-commit / post-merge

Hook handlers invoked automatically by Git after checkout, commit, and merge.
Not intended for direct user invocation.

```bash
crab lfs post-checkout <old-ref> <new-ref> <branch-checkout>
crab lfs post-commit
crab lfs post-merge <squash-merge>
```

These hooks ask Git which tracked files have both `filter=lfs` and `lockable`.
Files with an active LFS lock owned by the current Git user remain writable;
other lockable LFS files are made read-only.

---

## crab lfs checkout

Replace LFS pointers in the working tree with actual content.

```bash
crab lfs checkout [PATH...]
crab lfs checkout --to <OUTPUT> (--base|--ours|--theirs) <CONFLICT_PATH>
```

| Argument/Option | Description |
|-----------------|-------------|
| `PATH...` | Paths or glob patterns to check out |
| `--to <OUTPUT>` | Write one conflict stage to this output path |
| `--base` | Use the merge-base conflict stage |
| `--ours` | Use our conflict stage |
| `--theirs` | Use their conflict stage |

Like Git LFS, checkout uses objects already present in the local LFS cache.
Use `crab lfs fetch` or `crab lfs pull` first when objects are missing.

---

## crab lfs lock / unlock / locks

Advisory file locking for LFS-tracked files.

```bash
crab lfs lock [OPTIONS] <path>
crab lfs unlock [OPTIONS] [path]
crab lfs locks [OPTIONS]
```

| Command | Description |
|---------|-------------|
| `lock <path>` | Create an advisory lock on an existing working-tree file; supports `--json`, `--remote`, and `--expires-in` |
| `unlock <path>` | Remove a lock for an existing clean working-tree file; supports `--force`, `--json`, and `--remote` |
| `unlock --id <ID>` | Remove a lock by lock ID instead of path |
| `locks` | List active locks; supports `--json`, `--verify`, `--id`, `--path`, `--limit`, `--local`, `--cached`, and `--remote` |

`locks --local` reads Crab's local LFS lock cache and does not contact the
remote store. `locks --cached` reports the last unfiltered remote `locks`
result for the selected remote.
`locks --verify` contacts the remote store, marks locks owned by the current Git
identity with `O`, and marks stale local cache records missing from the remote
with `X`/`broken`; JSON output includes `ours`, `local`, and `broken` fields.
`--id`, `--path`, and `--limit` also filter verified output.
`unlock --force` skips the working-tree existence and clean-status checks.

These are Crab's direct object-storage lock commands. They do not implement
the Git LFS File Locking HTTP API, so stock `git lfs lock`, `git lfs unlock`,
and `git lfs locks` require an external LFS server.

---

## crab lfs ls-files

List LFS-tracked files and their status.

```bash
crab lfs ls-files [OPTIONS] [REF]
crab lfs ls-files [OPTIONS] <REF> <REF>
```

| Argument/Option | Description |
|-----------------|-------------|
| `REF` | List LFS files in the tree at this ref instead of `HEAD` |
| `<REF> <REF>` | List LFS files modified between two refs; deletions are not listed |
| `--all`, `-a` | List across all local history |
| `--deleted` | Include deleted LFS files from the selected ref history; cannot be combined with a ref range |
| `--long`, `-l` | Show full OIDs |
| `--name-only`, `-n` | Show only filenames |
| `--size`, `-s` | Include file sizes in the output |
| `--debug`, `-d` | Include full OID, version, checkout, and download details |
| `--json`, `-j` | Output stable JSON |
| `--include`, `-I` | Include only paths matching a pattern list |
| `--exclude`, `-X` | Exclude paths matching a pattern list |

---

## crab lfs status

Show staged and modified LFS-tracked files.

```bash
crab lfs status [--json|-j] [--porcelain|-p]
```

| Option | Description |
|--------|-------------|
| `--json`, `-j` | Output as JSON |
| `--porcelain`, `-p` | Machine-parseable output |

---

## crab lfs fsck

Verify LFS pointers and local LFS objects.

```bash
crab lfs fsck [REVISION|A..B] [--pointers] [--objects] [--dry-run]
```

| Option | Description |
|--------|-------------|
| `REVISION`, `A..B` | Inspect one commit-ish or one two-dot revision range |
| `--pointers` | Verify that LFS pointers in the selected revision are canonical |
| `--objects` | Verify only selected objects (skip pointer checks) |
| `--dry-run`, `-d` | Report corrupt objects without moving them to `.git/lfs/bad` |

Without a revision, object checks preserve Crab's local-store scan over
`.git/lfs/objects`. With a revision or range, object checks verify that each
referenced LFS object exists locally and matches its SHA-256 OID.
Corrupt local object files are moved to `.git/lfs/bad` unless `--dry-run` is
set. Missing objects and non-canonical pointers are reported but not moved.

---

## crab lfs prune

Remove unreferenced LFS objects from local storage.

```bash
crab lfs prune [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--verify-remote`, `-c` | Delete only candidates that are confirmed present in the remote store |
| `--no-verify-remote` | Disable remote verification |
| `--verify-unreachable` | Require remote verification for unreachable candidates |
| `--no-verify-unreachable` | Do not require remote verification for unreachable candidates |
| `--when-unverified <halt|continue>` | Halt or continue when a candidate cannot be verified remotely |
| `--recent` | Prune objects that are only retained by recent-ref protection |
| `--dry-run`, `-d` | Report what would be pruned without deleting |
| `--force`, `-f` | Skip confirmation prompts |
| `--verbose`, `-v` | Print full object IDs in prune output |

Prune protects objects referenced by the current checkout, stashes, other
worktree checkouts, the staged Git index, recent refs, recent commits, and
commits not pushed to the configured prune remote. The recent window follows
`lfs.fetchrecentrefsdays`, `lfs.fetchrecentremoterefs`,
`lfs.fetchrecentcommitsdays`, and `lfs.pruneoffsetdays`. `--recent` disables
only recent protection. `--force` skips the deletion confirmation; it does not
change which objects are protected.

With `--verify-remote`, prune HEAD-checks candidates in the configured LFS
object store before deleting them locally. Reachable candidates are always
verified; unreachable candidates are verified when `--verify-unreachable` is
set. Objects missing remotely are kept in `.git/lfs/objects`. Without that
flag, prune only reasons about the local cache.

With `--verify-remote`, Crab matches Git LFS and defaults
`--when-unverified` to `halt`. Without remote verification, the option defaults
to `continue` because no remote check can fail. When neither CLI override is
present, `lfs.pruneverifyremotealways` and
`lfs.pruneverifyunreachablealways` supply the verification defaults. Remote
checks are bounded by `lfs.concurrenttransfers` and processed in bounded
batches. Git revision and index discovery streams fixed-size object batches,
and `cat-file --batch-check` excludes non-blob and oversized objects before
their contents are read.

Prune honors `lfs.storage`; a relative value is resolved inside the common Git
directory, matching Git LFS. Do not prune when multiple repositories share one
custom storage directory. Crab locks concurrent prune runs, rejects malformed
or incomplete Git scans, validates each content-addressed object immediately
before unlinking it, and exits nonzero if any deletion fails.

---

## crab lfs convert

Convert files between LFS and crab-native pointer formats.

```bash
crab lfs convert --from lfs --to xet <PATH>
crab lfs convert --from xet --to lfs <PATH>
crab lfs convert --rollback
```

Direct conversion requires a clean worktree and updates only indexed paths that
match both the pattern and source pointer format. It preserves executable index
modes. LFS-to-Crab conversion streams and verifies the LFS object bytes, writes
them atomically into the working tree, and runs the native Crab add path so
chunk data and metadata are staged together. Crab-to-LFS conversion streams and
verifies hydrated bytes, atomically installs the local LFS object, uploads it to
the configured LFS store, and stages the LFS pointer without collecting a large
file in memory.

Conversion and rollback share an exclusive repository lock. Before changing
files, Crab atomically writes `.git/crab-lfs-convert-state.json`; a new
conversion replaces the prior completed conversion manifest. A failed
conversion automatically rolls back. `crab lfs convert --rollback` preflights
all affected files, refuses to overwrite post-conversion user edits, then
restores exact `.gitattributes` bytes, index blobs, executable modes, and source
pointer checkouts from the latest manifest. Candidate discovery streams the
Git index in fixed-size batches and prefilters non-pointer-sized blobs before
reading content; memory still scales with the matching files retained in the
rollback manifest, not with every indexed file.

---

## crab lfs migrate

Rewrite history to convert files to/from LFS pointers.

By default, Crab matches Git LFS by operating on the current branch and only on
commits that are not reachable from any remote-tracking ref. Crab refreshes Git
remote refs before selecting commits for `migrate import`, `migrate export`,
and `migrate info`; use `--skip-fetch` when the local remote-tracking refs are
already current. `--everything` includes all refs, while explicit
`--include-ref` / `--exclude-ref` filters bypass the default remote-ref
exclusions.

### crab lfs migrate import

```bash
crab lfs migrate import [--include <PATTERN> [--exclude <PATTERN>] | --above <SIZE> | --fixup] [--object-map <PATH>] [--everything] [--include-ref <REF>] [--exclude-ref <REF>] [--skip-fetch] [--yes] [--verbose] [--from-crab] [BRANCH...]
crab lfs migrate import --no-rewrite [--message <MESSAGE>] [--yes] [--verbose] <FILE>...
```

| Option | Description |
|--------|-------------|
| `--include <PATTERN>`, `-I` | Glob pattern for files to convert; if omitted, rewrite mode considers all paths |
| `--exclude <PATTERN>`, `-X` | Exclude files matching this pattern from conversion |
| `--above <SIZE>` | Only convert files whose individual size is at least this threshold; incompatible with `--include`, `--exclude`, and `--fixup` |
| `--fixup` | Convert files already tracked by existing `.gitattributes` `filter=lfs` rules; incompatible with `--include`, `--exclude`, and `--above` |
| `--no-rewrite` | Convert current-branch files in a new commit without rewriting history; rewrite/ref selection flags are accepted and ignored except `--fixup` |
| `--message <MESSAGE>`, `-m` | Commit message for `--no-rewrite` |
| `--object-map <PATH>` | Write a CSV mapping old commit IDs to rewritten commit IDs; accepted and ignored with `--no-rewrite` |
| `--everything` | Process all refs |
| `--include-ref <REF>` | Include commits reachable from this ref |
| `--exclude-ref <REF>` | Exclude commits reachable from this ref |
| `--skip-fetch` | Do not refresh remote refs before selecting commits to rewrite |
| `--yes` | Continue when the working tree is dirty and may be overwritten |
| `--verbose`, `-v` | Print the commit id and filename for each migrated file |
| `--from-crab` | Convert matching Crab pointer blobs to Git LFS pointers by reconstructing the Crab content and uploading/caching it as LFS objects |
| `BRANCH...` | Branches or refs to migrate; prefix with `^` to exclude |
| `FILE...` | With `--no-rewrite`, files to convert; each file must already match a `.gitattributes` `filter=lfs` rule |

### crab lfs migrate export

```bash
crab lfs migrate export --include <PATTERN> [--exclude <PATTERN>] [--object-map <PATH>] [--remote <GIT_REMOTE>] [--everything] [--include-ref <REF>] [--exclude-ref <REF>] [--skip-fetch] [--yes] [--verbose] [--to-crab] [BRANCH...]
```

| Option | Description |
|--------|-------------|
| `--include <PATTERN>`, `-I` | Glob pattern for files to convert back |
| `--exclude <PATTERN>`, `-X` | Exclude files matching this pattern from conversion |
| `--object-map <PATH>` | Write a CSV mapping old commit IDs to rewritten commit IDs |
| `--remote <GIT_REMOTE>` | Download missing LFS objects from this Crab Git remote |
| `--everything` | Process all refs |
| `--include-ref <REF>` | Include commits reachable from this ref |
| `--exclude-ref <REF>` | Exclude commits reachable from this ref |
| `--skip-fetch` | Do not refresh remote refs before selecting commits to rewrite |
| `--yes` | Continue when the working tree is dirty and may be overwritten |
| `--verbose`, `-v` | Print the commit id and filename for each migrated file |
| `--to-crab` | Convert matching Git LFS pointer blobs to Crab pointers by resolving LFS content, staging it in Crab, and adding Crab tracking rules |
| `BRANCH...` | Branches or refs to migrate; prefix with `^` to exclude |

Without `--to-crab`, export follows Git LFS by appending a `.gitattributes`
untrack override such as `*.bin !text !filter !merge !diff`. With `--to-crab`,
Crab removes the LFS rule for the selected pattern and adds `filter=crab`
tracking instead.

### crab lfs migrate info

```bash
crab lfs migrate info [--above <SIZE>] [--include <PATTERN>] [--exclude <PATTERN>] [--fixup] [--everything] [--include-ref <REF>] [--exclude-ref <REF>] [--skip-fetch] [--top <N>] [--unit <UNIT>] [--pointers[=<MODE>]] [BRANCH...]
```

| Option | Description |
|--------|-------------|
| `--above <SIZE>` | Only include files larger than this size (e.g. `1mb`, `500kb`) |
| `--include <PATTERN>`, `-I` | Only analyze files matching this pattern |
| `--exclude <PATTERN>`, `-X` | Exclude files matching this pattern |
| `--fixup` | Analyze files that match existing `.gitattributes` `filter=lfs` rules and are not already LFS pointers; incompatible with `--include`, `--exclude`, and pointer modes other than `--pointers=ignore` |
| `--everything` | Analyze all refs |
| `--include-ref <REF>` | Include commits reachable from this ref |
| `--exclude-ref <REF>` | Exclude commits reachable from this ref |
| `--skip-fetch` | Do not refresh remote refs before selecting commits to analyze |
| `--top <N>` | Show only the top N regular file entries; default is 5 |
| `--unit <UNIT>` | Format sizes with `b`, `kb`, `mb`, `gb`, `kib`, `mib`, or similar units |
| `--pointers=follow` | Count existing LFS pointers by referenced object size in a separate `LFS Objects` row; this is the default |
| `--pointers=no-follow` | Count existing LFS pointer blobs as regular files |
| `--pointers=ignore` | Ignore existing LFS pointers |
| `--pointers` | Legacy Crab shorthand: list existing LFS pointers in HEAD |
| `BRANCH...` | Branches or refs to analyze; prefix with `^` to exclude |

---

## crab lfs pointer

Generate, validate, or inspect LFS pointers.

```bash
crab lfs pointer [OPTIONS]
```

| Option | Description |
|--------|-------------|
| `--file <PATH>` | Generate the LFS pointer for a file |
| `--stdin` | Read a pointer from stdin and display parsed fields |
| `--check` | Validate the pointer (exit 0 if valid, 1 if invalid) |
| `--strict` | Reject non-canonical pointers (use with `--check`) |
| `--no-strict` | Accept valid but non-canonical pointers (use with `--check`) |

### Examples

```bash
crab lfs pointer --file models/weights.bin
echo "version https://..." | crab lfs pointer --stdin
crab lfs pointer --check --strict < pointer.txt
crab lfs pointer --check --no-strict < pointer.txt
```

---

## crab lfs clean / smudge

Standalone filter operations (stdin → stdout).

```bash
crab lfs clean [PATH]       # Clean filter: content → pointer
crab lfs smudge [PATH]      # Smudge filter: pointer → content
crab lfs smudge --skip      # Pass pointer through unchanged
```

These are invoked by git's filter driver, not typically by users directly.
`crab lfs install` configures Git to pass `%f` as the optional path operand.
`crab lfs smudge` honors `GIT_LFS_SKIP_SMUDGE`, `lfs.fetchinclude`, and
`lfs.fetchexclude`. When a path does not pass the include/exclude filters, the
pointer is copied to stdout unchanged instead of reading the local cache or
downloading from the remote store.
The clean filter only writes the verified object to the local LFS cache;
remote publication is performed by `crab lfs pre-push` or `crab lfs push`, so
`git add` remains usable without remote access.

---

## crab lfs filter-process

Long-running Git filter process endpoint for LFS-compatible clean/smudge
operations. It speaks Git's packet-line process filter protocol on stdin/stdout
and is intended to be run by Git, not directly by users.

```bash
crab lfs filter-process [--skip]
```

| Option | Description |
|--------|-------------|
| `--skip`, `-s` | Skip automatic smudge downloads and pass pointers through |

Crab reuses its canonical filter-process engine here. Paths with
`.gitattributes` `filter=lfs` rules clean to Git LFS pointers and smudge from
the local LFS cache or Crab's configured LFS object store. `--skip` and
`GIT_LFS_SKIP_SMUDGE` force lazy smudge behavior. `lfs.fetchinclude` and
`lfs.fetchexclude` also apply to process-filter smudge requests.

---

## crab lfs merge-driver

Git LFS merge driver endpoint for text LFS files. It is intended to be invoked
by Git's merge driver configuration, not directly by users.

```bash
crab lfs merge-driver \
  --ancestor <path> \
  --current <path> \
  --other <path> \
  --output <path> \
  [--marker-size <n>] \
  [--program <shell-command>]
```

Crab resolves pointer inputs from the local LFS cache or configured Crab LFS
object store, runs the merge program, then cleans the merged result back to an
LFS pointer in `--output`. Without `--program`, Crab uses Git LFS's default
merge command: `git merge-file --stdout --marker-size=%L %A %O %B >%D`.

---

## crab lfs standalone-file

Internal Git LFS custom transfer adapter endpoint. It speaks JSON lines on
stdin/stdout and uses Crab's configured LFS object store. This command is not
intended for direct user invocation.

```bash
crab lfs standalone-file
```

Crab's `install` command configures Git LFS to call `crab lfs-transfer-agent`
for normal operation; `standalone-file` is provided for Git LFS command-surface
compatibility and uses the same transfer protocol implementation.

---

## crab lfs env

Print LFS diagnostic environment information, including the direct Crab
object-storage remote and local transfer-agent configuration. Crab does not
report or require an HTTP LFS endpoint.

```bash
crab lfs env
```

---

## crab lfs version

Print crab LFS version information.

```bash
crab lfs version
```

---

## crab lfs dedup

Deduplicate checked-out LFS files with the local LFS object store.

```bash
crab lfs dedup
crab lfs dedup --test
crab lfs dedup --dry-run
crab lfs dedup --crab-cache [--dry-run]
```

| Option | Description |
|--------|-------------|
| `--test`, `-t` | Check whether copy-on-write file cloning is supported |
| `--dry-run` | Report working tree files that would be deduplicated |
| `--crab-cache` | Run Crab's cache cleanup mode instead of Git LFS-style working-tree dedup |

Default dedup follows Git LFS semantics: it requires a clean working tree, no
configured LFS extensions, and filesystem support for copy-on-write file
cloning. It re-creates checked-out LFS files as copy-on-write clones of their
objects under the configured LFS storage directory. Before any replacement,
Crab verifies every cache object and checkout against the indexed SHA-256 and
size and rejects symlinks and non-regular files. `--dry-run` performs the same
preflight without modifying files or requiring a copy-on-write probe.

With `--crab-cache`, Crab runs its older cache cleanup mode. That mode removes
only local LFS cache objects whose path SHA-256 matches their contents, whose
size and Blake3 hash match a Crab pointer reachable from the index or local
refs, and whose bytes are reconstructed identically from the local Crab staging
area. Unverified objects are skipped and remain in the local LFS cache.
Deletion failures make the command fail instead of reporting a partial cleanup
as successful.

---

## crab lfs logs

Display Git LFS-style error logs from `.git/lfs/logs`.

```bash
crab lfs logs
crab lfs logs last
crab lfs logs show <file>
crab lfs logs <file>
crab lfs logs clear
crab lfs logs --transfer-history [--last <N>] [--clear]
```

Without arguments, Crab lists error log files. `last` shows the most recent
error log, `show <file>` or `<file>` prints a specific log, and `clear` removes
error logs while preserving Crab's transfer-history file. Use
`--transfer-history` to view the Crab-specific fetch/push/pull transfer log.

## Related Commands

- [`crab track`](crab-track.md) — native crab file tracking.
- [`crab hydrate`](crab-hydrate.md) — native crab file hydration.
- [`crab lock`](crab-lock.md) — native crab file locking.
- [`crab migrate`](crab-migrate.md) — native crab history migration.
