# Crab use-case series image prompts

These are the final prompts used with the built-in image-generation workflow.
All images are standalone 16:9 social campaign graphics. Keep the shared visual
system consistent across the set: dark navy blueprint background, crisp off-white
type, cyan data paths, warm coral decision or mutation paths, subtle grid,
technical editorial infographic, generous safe margins, no mascots, no people,
no third-party logos, no photorealism, and no watermark.

## 00 — Choose the right data path

Use case: infographic-diagram. Asset type: social launch graphic. Create a
polished 16:9 technical decision diagram titled exactly “ONE REPO · THREE DATA
PATHS”. Start with a mixed repository at left and route three horizontal lanes
to one customer-owned object-storage bucket at right. Lane labels exactly:
“GIT BLOBS”, “CRAB LFS”, “CRAB NATIVE”. Show source code in the first lane,
one sealed whole-file object in the second, and content-defined chunks packed
into xorbs in the third. Make `.gitattributes` the visible routing switch. Minimal
text only; spell labels exactly. Avoid logos and provider branding.

## 01 — Code repository in your bucket

Use case: infographic-diagram. Asset type: social use-case graphic. Create a
polished 16:9 architecture diagram titled exactly “YOUR GIT REMOTE · YOUR
BUCKET”. Show a code worktree and Git commit graph passing through a compact
`git-remote-crab` gateway into an object-storage bucket containing “GIT PACKS”
and “GUARDED REFS”. Leave a clear open gap with the exact label “NO DATA SERVER”.
Emphasize a direct, simple path and customer ownership. Minimal text; no logos.

## 02 — Versioned model checkpoints

Use case: infographic-diagram. Asset type: social ML use-case graphic. Create a
polished 16:9 technical diagram titled exactly “MODEL LINEAGE · WITHOUT FULL
COPIES”. Show three checkpoint versions aligned to three Git commits. Break each
model into chunk bands: most cyan chunks repeat across versions while a few coral
chunks change. Shared chunks point to the same packed xorb blocks. End every
version with a small “VERIFIED” seal to show complete-file identity, not a patch
chain. Minimal exact text; no numeric savings claim.

## 03 — Stable binaries with Crab LFS

Use case: infographic-diagram. Asset type: social LFS use-case graphic. Create a
polished 16:9 diagram titled exactly “WHOLE FILE · STANDARD LFS POINTER”. Show a
compact standard Git LFS pointer card with “SHA-256” connected to one sealed,
verified whole-file object in a private bucket. Surround the source side with
simple abstract icons for video, encrypted archive, firmware, and compressed
design export. Communicate simplicity and compatibility rather than
deduplication. Minimal text; no third-party logos.

## 04 — Mixed repository routing

Use case: infographic-diagram. Asset type: social monorepo use-case graphic.
Create a polished 16:9 routing diagram titled exactly “MATCH THE ENGINE TO THE
FILE”. Show one repository tree with three folders: “SOURCE”, “MODELS”,
“RELEASES”. A visible `.gitattributes` switch routes source to “GIT”, models to
“CRAB NATIVE”, and releases to “CRAB LFS”. Show one commit spanning all three
outcomes. Make the three representations visually distinct but coherent. Minimal
text only; no logos.

## 05 — Lazy creative workspace

Use case: infographic-diagram. Asset type: social creative-workflow graphic.
Create a polished 16:9 diagram titled exactly “2 TB HISTORY · 40 GB WORKING SET”.
Show a large cloud archive of dimmed media, game, audio, and CAD blocks on the
left. A bright selection beam labeled “HYDRATE” picks one scene, texture group,
and audio group into a workstation on the right. Unselected items remain compact
pointer cards. Include a small reverse arrow labeled “DEHYDRATE CLEAN FILES”.
No people, logos, or claims beyond the title's illustrative sizes.

## 06 — CI working sets and cache

Use case: infographic-diagram. Asset type: social CI workflow graphic. Create a
polished 16:9 diagram titled exactly “EACH JOB GETS ONLY WHAT IT NEEDS”. Show one
Crab repository feeding three CI job cards: “SOURCE CHECK”, “MODEL TEST”, and
“RELEASE”. The first receives Git blobs only; the second receives one model and
one validation slice; the third receives a reviewed artifact manifest. A shared
box labeled “VERIFIED STAGE CACHE” connects successful outputs between runners,
while the bucket remains the origin. Minimal text; no vendor logos.

## 07 — Self-hosted repository browser

Use case: ui-mockup. Asset type: social product explainer. Create a polished
16:9 high-fidelity but generic repository-browser mockup titled exactly
“BROWSE THE BUCKET · NO SERVER-SIDE CLONE”. In one browser frame show a branch
picker, repository tree, code pane, compact diff, history, and blame controls.
Below it, show a stateless Rust service reading Git packs and metadata from an S3-
style object store. Add crossed-out silhouettes labeled “NO CLONE” and “NO LOCAL
DB”. Do not imitate or use any third-party brand or logo. Keep text sparse.

## 08 — Protected collaboration workflow

Use case: infographic-diagram. Asset type: social collaboration graphic. Create
a polished 16:9 workflow titled exactly “PROPOSE · REVIEW · CHECK · MERGE”. Show
an OIDC identity ring around a proposal branch flowing through four gates:
“PULL REQUEST”, “REVIEW”, “REQUIRED CHECKS”, “PROTECTED MAIN”. A stale or failed
path stops visibly before main; the successful cyan path advances the ref. Store
Git state and collaboration records together in an operator-owned bucket at the
bottom. Minimal exact text; no third-party logos.

## 09 — S3 tools over Git history

Use case: infographic-diagram. Asset type: social gateway architecture graphic.
Create a polished 16:9 diagram titled exactly “S3 CLIENTS · GIT HISTORY”. On the
left show three generic clients labeled “PYTHON SDK”, “DATA PIPELINE”, and
“MIGRATION TOOL”. They send signed S3 requests to a “CRAB S3 GATEWAY” in the
center. Use the exact example key “main/data/model.bin”. On the right, show one
Crab repository with branch, commit, and file tree. Split the result into “READ ·
PIN COMMIT” and “WRITE · CREATE COMMIT”. Add the exact callout “GATEWAY KEYS ≠
BACKEND CREDENTIALS”. The private backing object store sits below the repository.
Use protocol arrows and clear trust boundaries. No provider or forge logos.

## 10 — Private-cloud sovereignty

Use case: infographic-diagram. Asset type: social finale graphic. Create a
polished 16:9 architecture matrix titled exactly “YOUR IDENTITY · YOUR SERVICE ·
YOUR STORAGE”. Use three separate horizontal rows inside a private-network
boundary, with no arrows crossing between rows. Row 1: “CRAB CLIENTS” → “DIRECT
STORAGE” → “OPERATOR-OWNED OBJECT STORAGE”. Row 2: “BROWSER + GIT/LFS” →
“REPOSITORY HTTP APP” → the same storage. Row 3: “S3 CLIENTS + CI” → “S3 GATEWAY”
→ the same storage. Above rows 2 and 3, place “OIDC IDENTITY PROVIDER” with thin
authorization arrows to both service boxes. Inside storage, show four labeled
layers: “GIT”, “LFS”, “CHUNKS”, “COLLABORATION”. Beside the services show small
sidecars labeled “DISPOSABLE CACHE” and “TEMP FILES”. Minimal exact text; no
third-party logos or production-ready badge. The final refinement removed an
incorrect vertical connector between the two independent services while
preserving the rest of the image.
