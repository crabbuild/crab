---
illustration_id: 07
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 07-gearhash-content-defined-chunking.png
---

# Gearhash Content-Defined Chunking

Use case: scientific-educational infographic for a technical open-source launch post.

Primary request: redraw this as a crystal-clear explanation of how content-defined chunking reacts to three different local edits. The viewer must immediately see exactly what was inserted, deleted, or modified; which nearby chunks receive new identities; and where unchanged chunk identities resume.

Layout: one full-width `Original file` ribbon across the top. Beneath it, three equal comparison rows labeled `1. Insert`, `2. Delete`, and `3. Modify`. Every row begins from the same original content. Align reused chunks vertically wherever possible so identity preservation is visually obvious.

ZONES:
- Top original ribbon: six large, plainly separated chunks labeled `C1`, `C2`, `C3`, `C4`, `C5`, `C6`. Use the same distinct blue fill for all original chunks.
- Insert row: show a narrow amber byte segment visibly added between original content, labeled `+ inserted bytes`. Nearby output chunks become amber `N1` and `N2`. Farther right, unchanged blue chunks `C4`, `C5`, and `C6` reappear with the same labels as the original.
- Delete row: show a short original byte segment above the ribbon with a strike-through and downward removal mark, labeled `− deleted bytes`. The edited ribbon must be visibly shorter. Nearby output chunks become amber `N1` and `N2`. Farther right, unchanged blue chunks `C4`, `C5`, and `C6` reappear.
- Modify row: show one small amber patch inside the original content, labeled `modified bytes`. The total ribbon length stays the same. Only the local output area becomes amber `N1`; blue chunks `C4`, `C5`, and `C6` remain unchanged.
- A single vertical dashed line at the first reused `C4` in all three rows, labeled `boundaries resynchronize`.
- Small footer facts: `Gearhash CDC`, `64 KiB target`, `128 KiB cap`.

FLOW:
- Thin vertical identity guides connect original `C4`, `C5`, and `C6` to the identically labeled blue chunks in every edit row.
- Amber is reserved exclusively for inserted, deleted, modified, and locally rechunked bytes.
- Blue chunks with the same labels mean byte-identical reusable chunks. Never color changed chunks blue.

LABELS: Render only these labels, spelled exactly: `Original file`, `1. Insert`, `2. Delete`, `3. Modify`, `+ inserted bytes`, `− deleted bytes`, `modified bytes`, `C1`, `C2`, `C3`, `C4`, `C5`, `C6`, `N1`, `N2`, `boundaries resynchronize`, `unchanged chunks reused`, `Gearhash CDC`, `64 KiB target`, `128 KiB cap`.

COLORS: off-white background, pale gray grid, deep slate outlines, engineering-blue unchanged chunks, light-blue identity guides, amber edited bytes and new local chunks, navy footer facts. Color values are rendering guidance only; never display color names or hex values.

STYLE: technical algorithm blueprint; flat 2D; precise equal-height ribbons; large labels; strong alignment; generous whitespace. The insertion must add visible length, deletion must remove visible length, and modification must preserve length. No 3D, gradients, shadows, binary filler text, people, mascots, logos, extra chunk IDs, or unlisted size values.

Avoid: ambiguous arrows, curved spaghetti connectors, tiny byte cells, decorative icons, or any depiction where the edit itself cannot be located instantly.

ASPECT: 16:9 landscape.
