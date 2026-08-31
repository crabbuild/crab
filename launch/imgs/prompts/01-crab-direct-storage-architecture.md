---
illustration_id: 01
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 01-crab-direct-storage-architecture.png
---

# Crab Direct-Storage Architecture

Use case: infographic-diagram.

Primary request: completely redraw the reference as a concise, technically precise direct-storage architecture. The relationship between Shards and Xorbs must be unmistakable. Remove every duplicate box, decorative icon, crossing connector, and unnecessary intermediate node from the reference.

CORE TECHNICAL IDEA:
- Git packs contain Git history and Crab pointer blobs.
- Xorbs contain immutable packed chunk bytes.
- Shards contain reconstruction metadata that locates ordered chunk ranges inside xorbs.
- Both Git and large-file objects are written directly to the user's object store. There is no Crab data server in the middle.

COMPOSITION:
- One title: `DIRECT STORAGE ARCHITECTURE`.
- Exactly three aligned columns: `DEVELOPER`, `CRAB INTEGRATION`, `YOUR OBJECT STORE`.
- Exactly two horizontal lanes: a top Git lane and a bottom large-file lane.
- All column headings share one baseline. All nodes have equal heights, square corners, identical stroke widths, and centers snapped to one grid.
- All flow connectors are perfectly straight horizontal lines with centered arrowheads. No diagonal or crossing lines.

COLUMN 1 — `DEVELOPER`:
- Top node: `git`.
- Bottom node: `crab CLI`.
- Both nodes have identical dimensions and align vertically.

COLUMN 2 — `CRAB INTEGRATION`:
- Top node: `git-remote-crab`.
- Bottom node: `filter-process`.
- Both nodes have identical dimensions and align exactly with the corresponding Developer nodes.

COLUMN 3 — `YOUR OBJECT STORE`:
- One large clean boundary containing exactly two storage groups aligned with the lanes.
- Top group: one box labeled `Git packs + Crab pointers` with a small secondary line `history and file identity`.
- Bottom group: one box titled `Large-file data`. Inside it, place exactly two equal-width blocks on the same horizontal baseline:
  - Left block: `Shards`, with a small secondary line `reconstruction map`.
  - Right block: `Xorbs`, with a small secondary line `packed chunk bytes`.
  - Connect the right edge of `Shards` directly to the left edge of `Xorbs` with one short, perfectly horizontal arrow.
  - Place the exact label `locates chunk ranges` directly above that short arrow.
  - The arrow MUST visibly touch both blocks. There must be no gap at either endpoint.
- Do not draw separate or duplicated Shards or Xorbs anywhere else.

FLOW:
- Top lane: `git` → `git-remote-crab` → `Git packs + Crab pointers`.
- Bottom lane: `crab CLI` → `filter-process` → the outer `Large-file data` group.
- Within the large-file group only: `Shards` → `Xorbs`, labeled `locates chunk ranges`.
- Every external arrow must touch the exact center of its source and destination box edge.

FOOTER:
- One compact centered amber-outlined badge: `DIRECT ACCESS — NO CRAB DATA SERVER`.
- No server illustration and no crossed-out rack icon.

LABELS: Render only these labels, spelled exactly: `DIRECT STORAGE ARCHITECTURE`, `DEVELOPER`, `CRAB INTEGRATION`, `YOUR OBJECT STORE`, `git`, `crab CLI`, `git-remote-crab`, `filter-process`, `Git packs + Crab pointers`, `history and file identity`, `Large-file data`, `Shards`, `reconstruction map`, `Xorbs`, `packed chunk bytes`, `locates chunk ranges`, `DIRECT ACCESS — NO CRAB DATA SERVER`.

COLORS: solid off-white background, deep navy outlines and text, engineering-blue arrows and storage fills, pale blue node fills, amber only for the footer badge. Never display color names or color codes.

STYLE: flat 2D minimal blueprint infographic; no background grid; generous whitespace; high-contrast readable typography; precise engineering geometry. No 3D, gradients, shadows, perspective, people, clouds, logos, terminal icons, document icons, storage illustrations, server illustrations, decorative symbols, dotted lines, or extra text.

ASPECT: 16:9 landscape, presentation-safe margins, all labels fully visible.
