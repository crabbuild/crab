---
illustration_id: 08
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 08-continuity-aware-xorb-packing.png
---

# Continuity-Aware Xorb Packing

Layout: left-to-right packing pipeline. Ordered file chunks enter `XorbBuilder`; the output compares a scattered packing choice with a continuity-aware packing choice, then shows fewer contiguous range reads during hydration.

ZONES:
- Left: ordered chunk strip labeled `File chunks`.
- Center: decision block `XorbBuilder` with three verified policy tags: `64 MiB target xorb`, `1 MiB minimum run`, `25% dedup threshold`.
- Upper right comparison: `Scattered reuse` with many disconnected read arrows.
- Lower right comparison: `Continuity-aware packing` with one long contiguous run.
- Far right: `Range GET` and `LZ4` tags beside the packed xorb.

FLOW:
- Ordered chunks → XorbBuilder → two comparison outcomes.
- Highlight the continuity-aware route as the selected balance between reuse and sequential access.

LABELS: Render only these labels, spelled exactly: `File chunks`, `XorbBuilder`, `64 MiB target xorb`, `1 MiB minimum run`, `25% dedup threshold`, `Scattered reuse`, `Continuity-aware packing`, `Range GET`, `LZ4`, `Dedup + locality`.

COLORS: off-white background, pale gray grid, deep slate chunks, engineering-blue selected packing route, navy xorb container, light-blue contiguous run, amber scattered-read penalties. Color values are rendering guidance only; never display color names or hex values.

STYLE: rigorous storage-engine blueprint; flat 2D; aligned chunk cells; exact policy tags; make byte continuity and range count visually measurable. No 3D, gradients, shadows, people, mascots, vendor logos, or extra numbers.

ASPECT: 16:9 landscape.
