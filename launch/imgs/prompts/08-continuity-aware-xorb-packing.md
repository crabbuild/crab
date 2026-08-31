---
illustration_id: 08
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 08-continuity-aware-xorb-packing.png
---

# Continuity-Aware Xorb Packing

Use case: scientific-educational comparison infographic for a technical open-source launch post.

Primary request: redraw the deduplication-versus-continuity tradeoff so it can be understood in seconds. Compare the same ordered file under two storage-layout extremes. Make stored-byte reuse and hydration read cost separate, directly labeled outcomes.

Layout: a full-width ordered recipe at the top labeled `Same file recipe`, containing eight equal chunks `C1` through `C8`. Below it, two large side-by-side comparison panels of equal size.

ZONES:
- Left panel heading: `Extreme A — maximize reuse`.
- Left storage: four clearly separate containers labeled `Xorb A`, `Xorb B`, `Xorb C`, `Xorb D`. The ordered recipe points to alternating small ranges across all four containers. Use multiple separated arrows and label the consequence `Fewer uploaded bytes` and `Many range GETs`.
- Right panel heading: `Crab goal — reuse + locality`.
- Right storage: two containers labeled `Xorb A` and `Xorb B`. The same ordered recipe maps into two long contiguous runs with only two simple arrows. Label the consequence `Some bytes may be repacked` and `Fewer range GETs`.
- Center balance marker between panels: `Storage reuse` on the left end, `Read locality` on the right end, and `Balance` at the midpoint.
- Bottom facts, clearly separate from the conceptual comparison: `64 MiB target xorb`, `1 MiB minimum run`, `LZ4 by default`.

FLOW:
- Both panels start from the exact same ordered recipe `C1` through `C8`.
- In the left panel, connectors deliberately preserve recipe order while touching many separate xorb ranges.
- In the right panel, connectors preserve the same recipe order while touching two contiguous ranges.
- Make the causal relationship explicit: more scattered reuse means more range requests; more continuity means fewer range requests.

LABELS: Render only these labels, spelled exactly: `Same file recipe`, `C1`, `C2`, `C3`, `C4`, `C5`, `C6`, `C7`, `C8`, `Extreme A — maximize reuse`, `Xorb A`, `Xorb B`, `Xorb C`, `Xorb D`, `Fewer uploaded bytes`, `Many range GETs`, `Crab goal — reuse + locality`, `Some bytes may be repacked`, `Fewer range GETs`, `Storage reuse`, `Balance`, `Read locality`, `64 MiB target xorb`, `1 MiB minimum run`, `LZ4 by default`.

COLORS: off-white background, pale gray grid, deep slate recipe chunks, engineering-blue continuity-aware mapping, navy xorb containers, light-blue contiguous ranges, amber scattered connectors and range-request penalty. Color values are rendering guidance only; never display color names or hex values.

STYLE: rigorous storage-engine comparison blueprint; flat 2D; straight connectors; large readable labels; identical recipe in both panels; unmistakable contrast between many scattered reads and two contiguous reads. No 3D, gradients, shadows, people, mascots, vendor logos, 25% threshold label, or extra numbers.

Avoid: a generic XorbBuilder black box, unlabeled arrows, duplicated facts beside both panels, or any layout that mixes the dedup decision with compression.

ASPECT: 16:9 landscape.
