# Large files

Open **Large Files** from the activity bar to track, inspect, hydrate and
dehydrate Crab and Git LFS files. **Files** provides previews and file details;
**Activity** in the app header retains transfer progress and outcomes, including
when no repository is open; **Settings → Storage & Cache**
manages saved storage connections, cache inspection and prefetch presets.

## Content and pointers

Git stores a small pointer for a managed large file. Crab pointers identify
logical content with Blake3; Git LFS pointers use SHA-256. Chunk and xorb
metadata lives separately from the pointer and is available through file
details. Crab uses content-defined chunks, so unchanged chunks can be reused
after a small edit. Git LFS objects retain their LFS identity and routing.

A working file can contain full content, a pointer, or be missing. The hydration
inventory distinguishes those states. Cached content and working content are
different: a pointer on disk does not imply that its content is unavailable in
cache. Unavailable measurements must not be interpreted as zero.

## Track and publish

1. Open **Large Files → Track & Stage** and select the intended files.
2. Use the tracking wizard to stage them. Existing Crab/LFS attribute routing
   is preserved; newly tracked exact paths receive literal attribute rules.
3. Enter a commit message and commit the selected paths. Unrelated staging is
   retained.
4. Push to publish the Git commit and its required large-file data.

The ordinary Changes view also stages whole files through the configured Git
filters. Fetch/pull and lazy clone remain ordinary repository workflows; hydrate
the working files you need after synchronization.

## Preview without hydrating

Open a file in **Files**, then use its preview or **File details**. Previews
capture one version and use bounded native reads, including index and historical
versions. Opening a preview does not replace a working pointer.

Audio/video playback and seeking use that captured source. A first LFS read can
require verification of the complete object. Failures remain visible; **Reopen
media** captures a fresh version. Codec support follows Electron's decoder.
Format-specific input/output limits and the remaining qualification matrix are
recorded in [Desktop large-file completion](../large-file-support.md).

## Hydrate and dehydrate

For one pointer, use **File details → Hydrate to disk**. In **Large Files →
Hydrate**, select all eligible files, a pattern, folder, extension, preset or
custom exact paths. Both Crab and Git LFS are supported. File-tree context
menus and the command palette use the same verified repository transfer owner.
Local auxiliary worktrees use their own repository handle and selected storage
identity. For an SSH worktree, use **Open worktree as full workspace**; the
auxiliary SSH panel reports its remaining handle-ownership limitation.

Hydration streams content into a private temporary file, verifies its identity,
then publishes only if the captured working target is unchanged. Missing parent
directories are created after verification; a concurrently created file wins.
Dehydration verifies both the working content and saved reconstruction before
replacing a committed regular file with its pointer. Dirty files, changed index
entries and unavailable saved content fail visibly. Unrelated staging is retained.

Selection transfers confirm every dehydration and hydration at 1 GiB or
100 files. History and read-only snapshots do not expose working-tree
transfer controls. Local and SSH controls require the negotiated native methods;
unavailable services disable the relevant direction.

**Activity** shows actual work and provides cancellation. Navigating away or
reloading the view keeps the main-process operation alive. Cancellation during
setup prevents a late native send; admitted native work acknowledges cancellation
after cleanup. Files already published by a multi-file operation remain published.

Transfer selections and outcomes survive app restart. An operation without a
terminal receipt shows **Outcome unknown**; check the working files before using
**Retry transfer**. Retry uses current services, storage identity and eligible
paths, and asks for confirmation. It starts a new attempt; it does not replay an
old native session or undo published files. Activity keeps the latest 200 finished
jobs and every active job. A history-storage error blocks new transfers and offers
**Reload Activity** after writable storage is restored. Native chunk resume and
network-byte accounting remain open; bytes written are not a network measurement.

Expand a transfer in **Activity** to inspect **Captured source versions**. Each
record shows the literal path, content hash and logical size; ordinary Git
restoration also records the index mode. These records survive restart and show
what that attempt selected. They do not prove publication. An interrupted or
older attempt may have no recorded source for a path; check the working file.
A retry captures current versions in its own record and preserves the original.

Hydration, dehydration and index restoration record ownership of private scratch
files before writing payload bytes. After interruption, the next publication
recovers matching recorded scratch. Recovery preserves foreign or replaced files
and reports the retained path for inspection. A cleanup failure can occur after
content was published: Activity retains that published count and reports failure.
Interrupted counters are labeled **Last recorded progress**; check the actual
working files before retrying. Recovery does not infer ownership from filenames
or automatically resume an old download.

## Discard whole-file changes

In **Git → Changes**, discard a file from its row, its binary/pointer card,
selected rows, or **Discard All**. The palette also provides **Git: Discard File
Changes** and **Git: Discard All Unstaged Changes**. Every whole-file restore
requires confirmation and restores the current index version. Staged changes
and untracked files are preserved. No Undo is offered for this operation.

Crab/LFS content is reconstructed and verified through the same native owner as
hydration. Ordinary Git files retain Git's checkout conversions. The owner
rechecks captured working-file and index identities before publication and rejects
observed changes. Missing parent directories
are created only after preparation succeeds. Pending editor buffers must first
be saved or reloaded; clean editors remain reserved until restoration finishes,
then read the restored file. **Activity** retains actual publication counts and
errors, including partial multi-file outcomes. Cancellation leaves already
published files in place. Read-only snapshots and older native runtimes do not
admit this mutation.

## Storage presets and recovery

In **Settings → Storage & Cache**, save a prefetch preset with a name and
patterns. **Run** resolves the current Crab/LFS inventory, hydrates the eligible
exact paths and reports the resulting file count. It uses the selected
repository, including SSH repositories with verified transfers available.

If repository services fail, use the visible recovery error and **Retry reopening
repository**. A healthy sibling repository remains usable. Reader sessions are
revoked on recovery or storage-identity replacement; reopen details to capture
fresh content. Cache inspection/cleanup has its own Settings controls. Editable
cache budgets, offline retention and measured network accounting remain
work, rather than implied by those controls.

## Qualification boundary

Packaged macOS arm64 and owned S3-compatible fixtures qualify the Crab/LFS
track/publish/lazy-clone/hydrate/edit/synchronize/rename/removal workflow, nested
missing-file restoration, confirmed index restoration, pending-editor protection,
selected identities and bounded local/SSH recovery.
The [completion record](../large-file-support.md) separates that evidence from
restartable transfers, storage budgets, larger workload
profiles and cross-platform/provider release gates.
