# Large Files View

The Large Files view (labeled "Large Files" in the activity bar) is
the control center for Crab's large-file management. It handles
tracking, hydration, dehydration, and storage inspection for files
managed by the Crab chunking engine.

## Accessing the Large Files View

- Click the **Package** icon in the activity bar
- This view is specific to Crab — it manages files that are too
  large for standard git (models, datasets, binaries, media)

## Concepts

### Pointer Files

When a large file is tracked by Crab, git stores a small pointer
blob instead of the full content. The pointer contains:
- Content hash (Blake3)
- Original file size
- Chunk references

### Hydration States

- **Hydrated** — full file content is available locally
- **Dehydrated** — only the pointer blob exists; content is in cloud
  storage
- **Partial** — some chunks are cached locally

### Tracking

Files must be explicitly tracked by Crab before they're managed as
large files. Tracking tells the clean/smudge filter to handle them.

## Layout

The view has multiple tabs:

### Files Tab

```
┌─────────────────────────────────────────────────────────┐
│  [List] [Tree] [Card]    Filter: ________    [Refresh]  │
├─────────────────────────────────────────────────────────┤
│                                                         │
│  📦 model-00001.safetensors    4.2 GB    ● Hydrated     │
│  📦 model-00002.safetensors    4.2 GB    ○ Pointer      │
│  📦 dataset.parquet            1.8 GB    ● Hydrated     │
│  📦 training-data.tar.gz       12 GB     ○ Pointer      │
│                                                         │
│  ─── Untracked Large Files ───                          │
│  ⚠️  new-model.bin             3.1 GB    [Track]        │
│  ⚠️  raw-data.csv              800 MB    [Track]        │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

Features:
- **View modes** — List, Tree (hierarchical), or Card (grid with
  previews)
- **File type icons** — distinct icons for archives, images, video,
  audio, data files, ML models
- **Size display** — human-readable sizes
- **Hydration status** — per-file indicator
- **Untracked section** — large files not yet tracked by Crab,
  with a one-click Track button

### Storage Map Tab

Visual representation of storage usage:
- Xorb distribution across cloud storage
- Deduplication savings
- Storage cost estimates

### Hydrate Tab

Bulk hydration controls:
- **Hydrate all** — download all pointer files
- **Hydrate by pattern** — glob-based selection (e.g., `*.safetensors`)
- **Dehydrate all** — replace all hydrated files with pointers
- **Selective hydrate** — checkbox selection for individual files

Progress is shown inline with per-file download status.

### Datasets Tab

For repositories containing structured datasets:
- Parquet file previews (schema + sample rows)
- Dataset statistics
- Column-level metadata

## Operations

### Playing Large Media

Open an audio or video file in Files to play it with native media controls.
Playback and seeking use bounded byte ranges from one captured version, including
Crab and LFS pointers; the working file stays a pointer. A first LFS read can
verify the complete object before playback starts. Corruption and read failures
appear in the preview, and **Reopen media** captures a fresh version for retry.
Container and codec support follows the bundled Electron media decoder.

### Tracking a File

1. The "Untracked Large Files" section shows files above the size
   threshold that aren't tracked
2. Click **Track** next to a file (or select multiple + batch track)
3. Crab runs `crab add` which:
   - Chunks the file using content-defined chunking (gearhash)
   - Deduplicates chunks against the session/shard/DB index
   - Stages chunks in `.crab/staging/`
   - Writes a pointer blob to the git index

### Hydrating Files

For one working Crab or Git LFS pointer, open **File details → Hydrate to disk**.
The captured content streams to disk, verifies its whole-file identity, then
replaces the unchanged pointer. Activity shows bytes written and provides
cancellation; browsing elsewhere keeps the operation alive. A cancelled write
preserves the pointer, and detected external edits are not replaced. Reopen
details to inspect the working copy after completion.

The bulk controls below currently use the Crab-only CLI inventory. Git LFS bulk
materialization and its complete lifecycle remain under implementation.

1. Select files to hydrate (or use "Hydrate All")
2. Click **Hydrate** — the agent runs `crab hydrate`
3. Progress shows per-file download status
4. Hydrated files replace pointer blobs with full content
5. The file is now readable by any tool

### Dehydrating Files

1. Select hydrated files (or use "Dehydrate All")
2. Click **Dehydrate** — replaces content with pointer blobs
3. Frees local disk space while keeping files in cloud storage
4. Dehydrate skips dirty files (uncommitted changes)

### Push Flow

After tracking new files:
1. The **Push Flow Panel** appears showing staged chunks
2. Displays upload size estimate and xorb count
3. Click **Push** to upload chunks to cloud storage
4. Progress shows upload status with retry on failure

## Staging Stats

The view shows staging area statistics:
- Number of staged chunks
- Total staged size
- Segment file count
- Option to clean abandoned staging data

## Context Menu

Right-click a file for:
- Hydrate / Dehydrate
- Preview (opens format-specific preview)
- Copy pointer hash
- Show in Explorer
- Remove tracking

## Integration

- The Explorer view shows hydration decorations on pointer files
- The Changes view shows Crab-staged files in a separate section
- The Dashboard shows overall hydration progress
- Push operations coordinate between git push and Crab push

## Understanding the Data Model

### Content-Defined Chunking

When you track a file with Crab, it is split into variable-size
chunks using a gearhash algorithm. This means:
- Small edits to a large file only produce new chunks for the changed
  regions
- Identical content across files is stored only once (deduplication)
- Chunks are compressed and stored as "xorbs" in cloud storage

### Three-Tier Deduplication

Crab deduplicates at three levels:
1. **Session** — within the current add operation
2. **Shard** — against recently pushed data
3. **DB Index** — against the entire repository history

This means re-adding a slightly modified 10 GB file might only upload
a few megabytes of new chunks.

### Pointer Blob Format

A pointer blob stored in git looks like:
```
crab/v1
hash: blake3:abc123...
size: 4294967296
chunks: 1024
```

This tiny blob (< 200 bytes) replaces the multi-gigabyte file in git,
keeping the repository fast to clone and browse.

## Tips and Best Practices

- **Track early** — track large files before your first commit to
  avoid bloating the git history
- **Hydrate selectively** — only hydrate files you need to work with;
  keep the rest as pointers to save disk space
- **Dehydrate before pulling** — dehydrate all files before `git pull`
  to avoid merge conflicts on hydrated content
- **Use patterns** — hydrate by glob pattern (e.g., `*.py`) to get
  just the files relevant to your current task
- **Check staging stats** — monitor the staging area size before
  pushing to estimate upload time
