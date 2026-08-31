---
illustration_id: 16
type: infographic
style: blueprint
language: en
aspect_ratio: "16:9"
output: 16-mirror-mode-publication-order.png
---

# Mirror Mode Publication Order

Layout: ordered two-plane publication sequence. Crab’s data plane completes first, then the forge collaboration plane receives the Git push. A boundary marker states that the two remote operations are not one transaction.

ZONES:
- Left: `Developer` and `git push origin`.
- Pre-push step: `crab push --remote crab`.
- Upper center plane: `Crab data plane` → `Object storage` with completion gate `Data published`.
- Lower/right plane: `Forge collaboration plane` → `PR / review / CI`.
- Between planes: boundary label `Two remotes ≠ one transaction`.
- Final monitoring callout: `CI checks divergence`.

FLOW:
- The pre-push Crab data arrow must reach `Data published` before the Git push proceeds to the forge.
- Show a numbered order: 1 for Crab publication, 2 for forge push.
- CI observes both planes and checks their relationship.

LABELS: Render only these labels, spelled exactly: `Developer`, `1`, `crab push --remote crab`, `Crab data plane`, `Object storage`, `Data published`, `2`, `git push origin`, `Forge collaboration plane`, `PR / review / CI`, `Two remotes ≠ one transaction`, `CI checks divergence`.

COLORS: off-white background, pale gray grid, deep slate developer and forge structures, engineering-blue Crab-first path, navy object storage, light-blue collaboration plane, amber transaction-boundary warning. Color values are rendering guidance only; never display color names or hex values.

STYLE: precise ordered integration blueprint; flat 2D; causal arrow order must be unmistakable; forge is generic, without a vendor logo. No 3D, gradients, shadows, people, mascots, brand marks, or extra commands.

ASPECT: 16:9 landscape.
