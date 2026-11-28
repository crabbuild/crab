# Native Results

Results are workflow evidence, independent of queue bookkeeping. The workflow
crate owns result schema, validation, scalar identity, comparison semantics,
plot interpretation and storage. CLI, desktop and the optional repository
server compose those owners with their existing authorization and execution
boundaries. The Results workspace is desktop-only; the Git web UI has no Results
tab or route. Immutable HTML exports retain their standalone browser renderer.
Desktop use does not require a server or external tracker.

## Identity and lifetime

Every new experiment captures a schema-1 result snapshot. Scalar fields retain
their file and escaped JSON pointer, with missing/invalid/unavailable values
distinct from zero and null. A snapshot records observed values, source metadata
digest, input witnesses, resolved parameters, stage outcomes and content-addressed
evidence. Current labels and queue status are live projections.

Scalar capture marks integers outside JavaScript's exact range as unsupported;
the original metric/parameter bytes remain retained. Snapshot and inline-chart
validation reject unsafe wire integers, including output sizes and stage durations,
so consumers never silently round an admitted value. The CLI's structured
`workflow.results` schema derives from its typed payload and workflow-owned DTOs.

Objects live under `workflow/results/objects/<blake3>`, run snapshots under
`workflow/results/runs/<experiment-id>/<blake3>.json`. A discovery pointer under
`refs/crab/results/<experiment-id>` selects the sealed snapshot. These are object
store control records, not advertised Git refs. Readers pin the digest for the
whole operation, and never substitute a current workspace file for missing
historical evidence.

Reports freeze a comparison, selected values, plot configuration, frozen chart specifications, renderer identity, and required
rendering evidence. Their manifest digest identifies
`workflow/results/reports/<blake3>.json`; an independent immutable root lives
under `refs/crab/results-reports/<blake3>`. They survive source experiment rename,
reset, removal and force publication. Render-required bodies are independent of
the mutable experiment namespace. External model/checkpoint links do not promise
reproduction or retention without the necessary retained evidence.

Publication uploads verified bodies before publishing a root under existing
writer admission. Pull verifies bodies and manifests before atomically installing
the local discovery pointer/report. Interrupted transfers leave reusable
immutable bodies, never an acknowledged incomplete result. No general Results
GC or automatic retention duration is introduced. Active maintenance exclusion
and explicit removal behavior require regression proof before release.

Results CLI transfers currently use direct repository storage. The shared CLI
repository-store constructor classifies installed managed authorities before
selecting direct credentials; unsupported managed operations and malformed
profiles fail explicitly. This also covers workflow/artifact/status/cache and
experiment writes. Bucket-level construction and the existing managed-read
resolver retain their separate contracts. Managed Results reads through granted
SDK stores require real service qualification; direct-storage proof does not
establish that capability.

## Capture and reproduction

Input capture happens after checkout, parameter composition, copied-path overlays
and checkpoint application, before commands execute. Observed metrics/plots and
bounded logs are captured before temporary worktree cleanup for immediate and
queued experiments, including failed runs. Failed evidence does not manufacture
an applicable success workspace.

Manual `exp save` creates a `saved` result from its owned workspace snapshot.
Declared metrics, parameters, plots and outputs are observations at
save time. Those parameter values are not proven execution inputs. Saved results
contain no stage attempts, borrowed logs, checkpoints or reproduction specification.
Apply and branch creation use the saved workspace; artifact promotion still
requires successful execution evidence.
For manual saves, declared templates resolve through the authoring repository's
render resources, including `.crab/plots`; other observations still read the
owned saved copy. Only referenced regular template files are retained, under
immutable content names. Within private `.crab` state, only the `plots` subtree
is eligible; Git-private paths are excluded. Canonical resource paths prevent
traversal from bypassing this boundary. The Apply tree still excludes private
state. Execution and reproduction continue resolving templates in their isolated
recorded tree, without consulting the current authoring workspace.
Resolved custom templates that exceed capture budgets or cannot be read keep an
explicit unavailable identity; a same-name built-in cannot replace their semantics.
Captured scalar values remain available when the chart cannot be rendered.
The shared capture owner's `capture_issues` name the declared template and failed
constraint, then direct the user to correct it before saving a new observation
with `crab exp save` or running another experiment. Desktop RunDetails
render that same message. The selectable unavailable-template attachment retains
the complete original technical diagnostic in `reason`. Shared `NotCaptured`
chart guidance also requires a new save/run: refreshing cannot repair evidence
never retained in an immutable snapshot.

Selected-result workspace actions prepare an owned copy before touching the
destination. They verify retained final observations and admitted output witnesses
against the selected digest, using the executor's file/directory hash semantics.
For manual saves, parameter and workflow observations also describe the restored
bytes. Preview, Apply and branch creation share this guard. Destination preflight
rejects directory symlinks before deletions or writes; file restoration streams
through atomic sidecars. Checkpoint restoration retains its separate verified
payload owner and does not depend on a terminal workspace snapshot.

Desktop JSON command failures consume the CLI's structured error envelope and
preserve its code, details and source chain. Process failures before envelope
emission retain process diagnostics. Cache inspection keeps its separate partial
report policy. A refused Apply/Resume preview states that workspace files were
not changed, keeps diagnostics expandable, and explains how to try the action
again after recovery. **Refresh view** only refreshes Results reads.

Per-attempt input identities use the executor's admitted hashes, recorded
atomically with its resolved stage hash before execution or cache replay. They
include directory and produced data rather than rehashing mutable files after
execution. External locations are omitted from publishable input witnesses;
their content digests remain usable for comparison. Outputs keep their path,
hash and byte size for promotion verification. Stage summaries use the final
attempt, discard stale outputs after failure, and leave duration unavailable
when the current attempt has no observed execution timing.

Reproduction restores the source commit and retained input overlays through the
existing worktree owner. Planner-owned output classification distinguishes source
inputs from data regenerated by selected upstream stages, including directory
outputs and working directories. Unselected, frozen, conditional or persistent
producers leave their existing dependency bytes in the required input closure.
Explicit copied-path overlays and restored checkpoint outputs/metrics are also
initial inputs, even when selected stages own their output paths. The runner
passes those materialized paths to capture; later reproductions retain the same
initial bytes rather than substituting the base commit's files. Repeated paths
share the existing capture budgets. This uses the existing input attachments.
`ReproductionSpec.directories` retains the traversed closed trees, including
empty directories. Capture bounds these to 1,024 sorted unique identities.
Restoration validates paths and file ancestry, replaces captured roots in its
owned temporary checkout, then recreates directories and verifies/writes files.
Base-checkout file modes are collected before replacement. Directory children
absent from the captured tree cannot reappear from the source commit, and replay
descendants retain the same directory closure.
Declared runtime identity must match; unretained inherited
environments, shell runtime dependencies, external input bytes or local execution
configuration make exact reproduction explicitly unavailable. Credentials and
entire inherited environments/local configuration are not result payloads.
Ordinary execution/resume behavior remains unchanged. Resume states the actual
checkpoint and base commit; current HEAD with old parameters is not historical
reproduction. Artifact promotion remains the existing immutable-version CAS
operation, distinct from creating a Git branch from an experiment.

Reproduced runs retain the immediate source run ID and immutable result digest
in `reproduction.source_result`. Captured run identities expose this relation;
reports freeze it independently of later source names or metadata. Original runs
have no source-result relation. A comment-only or empty local-settings document
does not change execution and permits reproduction; nonempty private settings
remain excluded from evidence and make exact reproduction unavailable.

## Explicit metadata migration

`crab results migrate <id>...` or `crab results migrate --all` explicitly converts
supported v1 experiment metadata into canonical metadata-only Results. The
existing `ExperimentMetadata::from_json` owner verifies schema and requested ID;
its v1 contract remains unchanged from `origin/main` and is present in tagged
`v2.0.0-alpha.1`. No release tag owns the new Results module. Its snapshot/catalog
DTOs, SDK reexports, generated client declarations and structured CLI schema
require `ResultOrigin`; retired Results shapes have no default origin, fallback
decoder or on-demand projection.

The pure workflow-owned conversion preserves recorded metrics, reported terminal
status (`success`, `failed`, `saved`), base commit, name, timestamps and canonical
source-metadata digest. `ResultOrigin::ExperimentMetadata` forbids parameters,
stages, attachments, plots and reproduction. Current files, current parameters,
metadata parameter overrides and saved CLI arguments cannot supply those missing
facts. Reported success does not prove execution completion; imported `saved`
metadata does not establish a captured workspace. Original metric/plot/artifact
bytes, resolved inputs, logs and replay commands are not manufactured.

Scalar deltas remain recorded observations. Dataset/split/evaluator claims still
require a retained input witness; fabricated matching hashes are rejected before
comparison. Imported candidates or baselines never acquire improvement coloring.
Scalar-field-count overflow refuses migration rather than truncating observations,
because imports have no retained original metric files for omitted fields.

The CLI preflights all selected sources before installing roots: regular files,
8 MiB documents, at most 10,000 IDs and a 256 MiB source-plus-sealed batch.
`LocalResults::install_if_absent` preserves and verifies an existing canonical
capture, is idempotent for an existing valid selection and refuses corrupt roots
or bodies. Installation uses the existing immutable writer and tempfile 3.27's
`persist_noclobber`: a concurrent root is never overwritten. This is create-only
publication, not a claim of universal filesystem atomicity or a transactional
multi-root batch; interrupted installations can be retried.

Both catalogs mark reported status as imported. Shared action policy and CLI
digest guards deny Apply, branch, Resume, Reproduce and artifact promotion from
metadata-only Results. The tagged experiment commands' existing no-digest owned
workspace path remains separate and unchanged. Publishing recorded observations
and inspecting immutable reports do not add execution evidence. Shared visible
guidance leads with the absent capture and the need to run or save a new
experiment in working/immutable and local/remote contexts; contextual report
and local-worktree restrictions follow. Desktop button tooltips give that same
per-action reason priority. Cold report retrieval retains imported origin and
scalars independently of the source metadata and entire prior local Results cache.

## Comparisons and clients

The native Results tab uses the full Data Workflows canvas; the workflow-file
list returns in Editor, Status and Queue. Metrics and plots precede the selected
run's evidence inspector, with synchronized curves first. Wide layouts pair
charts; narrower layouts stack them. Comparison notes open from metric info controls.
Action eligibility, complete identities and range guidance use inline disclosures;
imported provenance remains visible. Status icons supplement text, and compact
zoom controls retain explicit accessible names. Column selection uses the shared
portalled popover, so it remains usable inside the scrolling workspace.

Read lifetimes use native repository/session IDs. Display metadata and capability
refreshes preserve the comparison's controls and keyboard focus; an actual scope
change still cancels and revalidates reads. Capabilities remain live.
Queue fetches apply only their latest request's rows/loading, so an older mount
reply cannot hide a newly enqueued experiment. Effect cleanup invalidates prior
queue reads.

Default SVG guide paint inherits the current viewer foreground, independently
of result reads. Light/dark transitions repaint titles, ticks, axes and legend
labels without rebuilding focused controls or changing saved ranges. Authored
guide configuration overrides these weak defaults; ordinary theme changes keep
authored data-series colors. The same renderer source supplies desktop and bundled offline
views.

Absolute delta is candidate minus baseline. Relative delta divides by the
baseline's absolute value and is unavailable for zero. Improvement coloring
requires declared minimize/maximize direction and equal dataset/split/evaluator
identity. Unknown provenance stays unknown; a numerical difference does not
establish significance.

Column selections distinguish automatic discovery (`null`) from an explicitly
empty selection (`[]`) in both queries and comparison specifications. Picker
changes update the query and comparison together; hiding the final metric does
not restore the default metric set. Saved reports freeze this selection and the
dataset/split/evaluator identities, which remain inspectable as read-only fields.

Queries rebuild a verified catalog from canonical objects, then filter and sort
before bounded pagination with stable ID tie-breaking. Scans stop explicitly at
10,000 roots or 256 MiB of manifests; remote reads have 16-way concurrency and
remain subject to the SDK/HTTP admission budget. No competing index authority is
introduced.
Cursors bind catalog generation and query. Initial limits are 200 rows/page,
32 selected scalar columns and 20 comparison runs/series. Curve overlays share
a 2,000-point display budget across all selected runs and y fields, with at most
2,000 points per series and a 2 MiB normalized response. A full 20×2,000-point SVG
probe took 2.3s to initialize and 790ms to zoom; sharing the display budget keeps
adding runs from multiplying rendering and nearest-point hit-region cost.
Sampling preserves endpoints, extrema, original row ordinals and both sides of
missing intervals. Missing/null values remain gaps; literal category text stays
text. `ChartReduction` distinguishes `sampled` from `counted`; desktop and
offline views use that shared contract for their notices. Sampled curves disclose
displayed versus retained point counts and keep source bytes. Summary metrics
always use captured values, not plotted samples.

Comparison reads cache verified plot bodies or their original shared read errors
for the operation. Each revision's primary data, optional cross-file `x_path`
coordinates and retained template verify before its observations enter the
chart. Missing, corrupt or denied evidence skips that revision with classified
recovery guidance. Captured scalar rows, verified surviving curves and their
coordinate domain remain visible. Local and remote regressions cover missing
and corrupt primary/coordinate bodies; source-classified failures preserve their
original errors without exposing provider diagnostics in renderer data or adding
a public error enum/DTO shape.

A partially available chart can retain a specification; an entirely unavailable
chart has none. Missing, uncaptured, corrupt or denied retained templates skip
their revision before template identity or matrix-counting policy is selected.
Only admitted revisions choose the template and contribute observations; an
unavailable revision never borrows a verified revision's template. Non-template
declaration incompatibility, differing admitted template identities or bytes,
parse failures and unsafe statistical sampling still refuse the affected chart
visibly. Classified partial-template availability is covered; this does not
relax authored transform semantics or the existing comparison limits.

Both React clients clear the visible chart container on a specification change
and render into a host owned by that generation. A late renderer can finish only
in its detached host, so unavailable evidence cannot leave old SVG curves visible.
Vega finalization cancels timers, listeners and tooltips but does not remove SVG;
the clients own that DOM lifecycle. Immutable offline exports have no equivalent
comparison-update path. The existing empty chart-host tab stop is a nonmaterial
source-only blemish, not an approved interaction change or a new system rule.

Authored plot evaluation can fail on first render or after a template control is
used. Vega logs these failures even when `runAsync` resolves, so the shared
renderer observes its error logger without throwing from event dispatch. It
finalizes the failed view, disconnects its resize observer and clears partial
paint before rejecting the initial render or notifying the existing caller.
Desktop, immutable HTML and workspace metrics HTML show a plot-local
alert. Runtime failures direct the user to inspect the template and source data;
existing syntax diagnostics retain their dependency wording.

Failed views lose their bindings and chart export actions; React range controls
also disappear. Immutable HTML removes the failed view from linked cursor/range
dispatch. Healthy sibling charts, their controls, captured scalars, immutable
report identity and retained CSV/template evidence remain available. Reopening a
report renders its frozen specification again; a rendering failure never rewrites
the recorded observations or evidence.

Cancellation and typed read-admission rejection still stop the whole operation.
The reader uses `crab_storage::read_rejection`, including through I/O wrappers,
before considering a read failure partial chart availability. An exhausted plot
read budget must never become a successful comparison with an unavailable chart.
Report save/publication continues to verify its required evidence closure;
viewing partial data does not make unavailable bytes publishable.

Plots that require all rows for authored transformations or aggregation refuse
oversized input visibly instead of applying statistics to a sample. Bounded
transformed/compound plots retain their authored semantics and controls. An
overlay also refuses different retained template bytes under the same path.
The shared parser keeps tagged `v2.0.0-alpha.1` CLI JSON string rows unchanged;
nonserialized absence/ordinal witnesses drive chart normalization and travel
with cross-file x coordinates.

Retained templates emit a distinct series for every selected y field of every
run. Long rows keep the selected source columns under their authored keys, so
calculations can still compare columns. Template metadata uses unoccupied names
rather than replacing source columns. Chart identity includes both the optional
plot ID and source path; a shared plot ID cannot collapse distinct source files.
DVC anchors insert typed JSON for data, encodings, grouping, tooltips, interaction
and dimensions. Unknown/unclosed anchors fail visibly; anchor-looking captured
values stay literal data.

Literal dots, brackets and backslashes in field names remain source keys.
Encoding accessors escape punctuation and use private coordinate aliases where
Vega-Lite requires them. Ordinary axes and tooltips retain authored labels;
aggregate/bin/time-unit definitions retain statistical titles such as
`Mean of loss`. Horizontal bar templates keep numeric x and categorical y,
with offsets separating selected runs and fields within each category.

Built-in smoothed/minimal curves distinguish selected revisions. Built-in
`confusion` and `confusion_normalized` matrices count every retained observation
before browser rendering. The shared owner consumes each run's parsed rows into
weighted class-pair counts; every selected y field of every run remains a
separate series. Raw matrices display those exact counts. Normalized matrices
divide each count by the sum across x for that y class within its own series,
with zero for an empty total. Aggregation and zero-cell imputation preserve the
series identity; statistics never pool runs or selected y fields.

Matrix keys use numeric class codes backed by typed normalized identities.
Numerically equal classes, including negative zero, group together and sort
numerically. Missing/null observations display as `(missing)` and remain distinct
from the literal string `"null"`; delimiter-bearing strings cannot collide as
class-pair keys. Literal class/run labels, including `datum.value` and
`datum.label`, live in parameter values rather than expression text. Tooltips
name `X (<label>)` and `Y (<label>)` separately from `count` and `normalized`,
including when a source dimension itself uses one of those statistic names.

The 2,000-cell bound applies to the expanded x-class × y-class × series grid,
including every selected y field and run, before Vega imputation. It does not
bound retained input observations. Counted notices report all observations and
the observed class-pair count; imputed zero cells can make the rendered grid
larger than that pair count. Combinations within the observed class domains with
no rows receive zero-count cells. An oversized grid refuses visibly and keeps
retained source evidence; it is never sampled into different statistics.
Built-in facets use equal width/height. Only owned built-in matrix names select
this counting path: a retained authored override, even one named `confusion`,
keeps its own specification and the existing refusal when oversized transforms
cannot safely be sampled.

Exported chart viewports are named and keyboard focusable. Vega owns the inner
host's ARIA attributes; the surrounding viewport owns scrolling. Export controls
and headings stay outside the pan area, so inspecting an authored compound or
faceted layout does not move its controls out of view.

Simple retained line charts expose a coordinate identity over the authored x
definition and complete observed/authored domain. Compatible charts share cursor
and numeric range controls. The shared renderer derives one scalar cursor from
the owned single-field selection tuple; desktop and offline consumers
use it without depending on Vega-Lite's rewritten selection-object keys.
For simple encodings without authored configuration, the hidden hit layer groups
retained coordinates by x and uses a constant y; series retain all sampled marks
while zoom rebuilds one hit region per x. Authored channels and configuration
keep complete sample metadata and two-dimensional hit regions for tooltips.
ComparisonSpec.axes stores up to 32 finite ranges,
validated against chart domains when saving. Runtime zoom uses Vega's domainRaw;
clearing it restores authored zero/nice/bindings. Reports freeze ranges into their
chart parameters. Report inspection keeps overrides only in memory; Reset
restores frozen ranges. Transformed, compound and categorical custom axes do not
acquire unproven numeric zoom semantics. Authored selections retain their unit
view and interaction signals without injected layers. Other template parameters
survive; conflicts with owned interaction names fail visibly.

Report transport/export retains metric/plot/image/template/workflow bodies and
optional logs. Inputs, model outputs and checkpoints remain immutable links with
explicit preview/download guidance, even if their bytes exist in a local cache.
Publishing a report still refuses content excluded by output push/remote policy.
HTTP publication uses the same retained-body policy; missing linked model/input
payloads do not prevent publication.

Working UI selections are validated repository-scoped SessionManager preferences.
Saved comparisons are reports recoverable from object storage. Session identities,
credentials and result payload caches are not preference state. Remote read-only
sessions can inspect evidence without a local YAML; writes require existing
writable-workspace capabilities. Actual bare remote sessions can open published
reports in this workspace; Publish report stays disabled without that capability.

Desktop Results lives inside Data Workflows, beside Editor, Status, and Queue.
Its tab remains reachable when workflow YAML is absent or invalid; Results has
no separate sidebar or activity-bar entry.

Retrieved reports lead with their title, run count and comparison when the local
catalog is empty. Comparison tables retain the run column during keyboard or
horizontal scrolling. Execution availability is explained in visible text.
Curve legends use concise, collision-checked run labels; full identities remain
available in run evidence. Unit/layer charts fit the available panel width with
legends below. Authored facet and concatenated layouts keep their own geometry.
Categorical curve overlays use distinct dash patterns as well as color. Forced
colors use live system colors for chart labels and axes; dashed series and their
legend symbols remain distinguishable in monochrome.

Desktop and CLI HTML use bundled compatible Vega assets. Rendering uses the
expression interpreter and a restricted resource loader instead of weakening
host CSP or fetching datasets/scripts from a CDN. Dependencies, asset generation
and installed-CLI offline behavior must be verified before acceptance.

Offline table and attachment labels include the full run ID alongside optional
names. UUIDv7 time prefixes cannot identify neighboring runs, and duplicate names
must not make Download or Preview ownership ambiguous.
Report identities and frozen manifest text wrap within narrow viewports; tables
and authored compound/faceted charts retain their own internal scrolling.
The desktop workspace lets operation-error text shrink and wrap long words while
**Refresh view** keeps its control width. Evidence-preview failures retain the
selected report and its frozen comparison; working-comparison plot notices remain
separate from the operation-error banner. Refresh view refreshes Results reads.
After transport recovers, reopening a retained attachment retries its actual
evidence read without replacing captured values.

The repository server's shared admission owner retains its 30-second response
budget, permits, cancellation and timeout response shape. The installed HTTP
dependency's safe-method contract selects “The read request timed out. Retry the
read.” for safe requests; mutations keep the existing possible-completed-write
warning. Results has no separate timeout policy. Current-grant revalidation after
an evidence read can instead report temporarily unavailable repository permissions
with HTTP 503. Neither response discards the frozen report, scalar observations,
verified curves or previously retrieved evidence.

## Delivery evidence

Plan `plans/020-native-results-workspace.md` owns the full acceptance matrix.
Implementation and focused proof alone do not establish completion of packaged
client, cold retrieval, authorization, maintenance, offline rendering, scale or
native platform qualification. Record those results separately as they are executed.

The preceding template/cursor correction passed source-bound Darwin-arm64
qualification against an owned RustFS fixture: all 60 capability checks and 42
required cases, plus the separate 10,000-run catalog check. Its pre-run
source/runtime binding and packaged CLI, sidecar and application digests matched
that completed evidence; independent evidence verification passed. This is
historical qualification for the earlier source, not proof of the counted-matrix
correction. The detailed
record is `.impeccable/review/results-template-parity/documentation-review.md`.
This platform/provider qualification does not establish general template parity,
support for large transformed inputs, managed-service access, other OS packages,
full Plan 020 completion or release readiness.

The counted-matrix correction has focused owner proof: 52 passed with three
intentional evidence/declaration producers ignored, including five matrix cases;
ten chart behavior cases and four real offline browser cases passed separately.
The matrix browser case verifies all 400,000 observations from two 100,000-row
sources and two selected y fields per source, exact counts/normalization, zero
cells and literal identities with network denied. An actual external-process CLI
test verifies counted Vega export while retaining all 10,000 original tagged
JSON string rows. Schema generation and minimal/results SDK checks passed.

The counted-matrix correction also passed source-bound Darwin-arm64
qualification against the owned RustFS fixture: all 64 capability checks and 49
required cases, plus the separate 10,000-run catalog check. Three packaged
Electron tests and six browser tests passed. The desktop comparison test verifies
eight retained observations, four observed pairs and eight rendered matrix cells
before and after restart and cold remote report retrieval; navigation remains
Data Workflows → Results. That run's source/runtime binding and actual CLI,
packaged CLI, sidecar and application digests match the completed evidence.
Packaged digests also match the pre-run record. Independent evidence verification
passed. This is historical qualification for the counted-matrix source; it does
not qualify the subsequent Results failure-state correction.

That run qualifies its recorded platform/provider capability matrix. Its visual
**ship** verdict is scoped to the matrix correction and supplied adjacent offline
charts. The historical record is
`.impeccable/review/results-counted-matrices/documentation-review.md`.

The failure-state correction has focused shared-owner proof: 54 passed with
three intentional evidence/declaration producers ignored, including local and
remote missing/corrupt primary-body curve preservation and classified unavailable
charts without provider diagnostics. The filesystem-backed SDK test first reproduced
swallowed admission rejection; it now passes, proving a one-request budget can
read the run manifest but rejects the additional comparison plot read with
`LimitExceeded`. Four evidence-verifier tests also passed.

The failure-state correction passed a canonical unchanged-source Darwin-arm64
recheck against the owned RustFS fixture: all 72 capability checks and 52 required
cases, plus the separate 10,000-run catalog check. All three packaged native
journeys, six browser cases, the real browser/HTTP/storage journey and current
HTTP grant checks passed. That run's source/runtime and actual debug CLI,
packaged CLI, sidecar and application archive hashes match the completed
evidence; packaged hashes also match the pre-run record. Independent evidence
verification passed. That run's source-tree binding is
`d8a978d7af1cd57349e6b38cffd0334b34ebf05e54b732a66cbd3c508dc7fa4e`,
with renderer runtime SHA-256
`af20a84eb867bbf7357425e37a583672c669568f0435c20f315e111d513fc54e`.

An earlier canonical attempt timed out after five seconds during cold report
retrieval while Retrieve remained busy. Its cause is unproven and its failure
artifacts remain retained. Three diagnostic packaged repetitions passed the
original five-second assertions, with actual retrieve-handler times of
311/540/701ms. No source, canonical assertion or deadline changed before the
successful recheck. That result does not establish a retrieval timing SLA or
resolve the earlier flake.

That correction's visual packet contains eight captures: seven native captures
from the third timed diagnostic repetition and one 390px browser failure capture from a
fresh two-case run over a CLI-produced report with a controlled API failure.
They are distinct from canonical full-run captures and replace the earlier
pre-final-source packet for that earlier review. Its scoped visual disposition is
**ship**, with no material fixes, for the captured failure/partial-data layouts.
Its named common-curve cases did not qualify partial cross-file coordinate,
authored-template or transform failures. Coordinate preservation is qualified
separately below; authored-template/transform variants remain follow-up work.
The detailed record is
`.impeccable/review/results-failure-states/documentation-review.md`. General
DVC/template parity, unsafe large authored transforms, managed-service access,
other native OS packages, full Plan 020 completion and release readiness remain
open.

The subsequent coordinate-failure/chart-removal correction passed source-bound
Darwin-arm64 qualification against the owned RustFS fixture: all 75 capability
checks and 52 required cases, plus the separate 10,000-run catalog check. All
three packaged native journeys, six browser cases and the real HTTP/storage/browser
journey passed. The source-tree binding is
`a270d17a523bd723d3707340b902d6df1cdf326e36eeb8a67dad2193b556d062`;
the renderer runtime remains
`af20a84eb867bbf7357425e37a583672c669568f0435c20f315e111d513fc54e`.
Independent reconciliation reproduced the current source digest and all four
actual CLI/package hashes; the packaged CLI, sidecar and application archive
also match the pre-run binding. The earlier 72/52 and 64/49 records remain
historical for their own sources.

The owner regression first reproduced a corrupt coordinate source removing a
healthy curve. The packaged regression then reproduced four stale SVG paths
where three verified paths were expected. Final native proof preserves three
paths when one primary/coordinate source fails, two independent paths when both
coordinate sources fail, and four paths after restoring verified bytes. Actual
bare remote report inspection retains Data Workflows → Results and disables
publishing. Browser controlled-response availability/recovery checks already
passed before this correction; they do not reproduce a browser stale-SVG defect
or prove an actual storage outage.

The final six-capture packet contains five native captures at 1280px/900px and
one 390px browser capture. Native captures come from an external copy of the
canonical journey with original assertions and real Close toast clicks before
screenshots; the browser capture comes from the current canonical controlled
comparison response over an actual CLI report. All dimensions and hashes match
the manifest. The scoped visual disposition is **ship**, with no material fixes;
it does not canonize the pre-existing empty-host tab stop. Focused owner proof
passed 54 tests with three intentional producers ignored; the filesystem-backed
SDK admission/cancellation test, client types, native test types, scoped lint,
UI/web builds and 399-page/4,308-fragment link check passed. The detailed record is
`.impeccable/review/results-coordinate-failures/documentation-review.md`.

This correction does not resolve the earlier unexplained five-second cold
retrieval timeout. Partial authored-template/transform failure variants, actual
provider outages and in-flight transitions, managed-service access, other native
OS packages, CI/release proof and the full Plan 020 gates remain open.

The saved-workspace extension has focused proof for fresh `exp save` observations
and guarded selected-result actions. Four actual CLI cases pass: truthful saved
observations, corruption refusal before workspace mutation, external-file
preservation across directory symlinks and ordinary directory-output restoration.
Existing Apply/deletion, saved-workspace and branch integration tests pass, as do
the checkpoint sibling's ordinary-success/symlink-refusal control and nine atomic
materialization tests, including interrupted-stream preservation. Both client
typechecks, 28 preference/session tests, four evidence-verifier tests and docs
links pass. Desktop ESLint remains unavailable because this checkout has no
ESLint configuration; no configuration repair is included.

Fix-round source-tree binding
`4346b1abd02f5bca1013cbdf80d78d56192d0defe1351cb2cb913e99be083229`
passes the real CLI storage journey and all three packaged native journeys on
Darwin-arm64 with the owned RustFS fixture. The renderer runtime remains
`af20a84eb867bbf7357425e37a583672c669568f0435c20f315e111d513fc54e`.
Independent reconciliation matches current source, runtime, actual CLI, packaged
CLI, sidecar and application hashes to the scoped binding and capture manifest.
This proof includes saved metric/curve display, correct disabled execution
actions, actual model/parameter restoration and a corrupt-copy Apply refusal.
Keyboard Diagnostics retains the semantic error. Refresh view leaves the model
unchanged, another Apply still refuses before repair, and restoring the original
captured bytes permits a successful Apply. Separate owner/process/cache tests
and a live busy-cache cleanup test pass; semantic command errors do not change
the cache owner's partial-inspection policy.

All four current native captures at 1280/900 × 1000 match their manifest hashes
and dimensions. They preserve Data Workflows → Results, neutral semantic surfaces,
compact controls, visible focus and internal scrolling. Initial finishing review
identified raw tracing JSON in the primary refusal and a misleading Retry label.
The scoped verdict now records both resolved: concise refusal/preservation/
recovery text with expandable Diagnostics, and correctly named Refresh view.
Its **ship** disposition applies only to those two findings, not the whole Results
surface or Plan 020. The detailed record is
`.impeccable/review/results-saved-workspace/documentation-review.md`.

Full source-bound qualification on the same saved-workspace source now passes
all 84 required capabilities and 61 required cases, plus the separate 10,000-run
catalog check, for Darwin-arm64 with the owned RustFS fixture. Three packaged
native journeys, six browser cases, the real browser/HTTP/storage journey and
current HTTP grant checks pass. Archived and completed evidence are byte-identical;
independent verification matches the current source/runtime and all four actual
artifact hashes to the pre-run full binding. The four scoped captures retain
their original pixels and hashes; corrected per-image provenance now describes
their actual states. This full proof qualifies the recorded platform/provider
matrix without expanding the visual verdict beyond its two scored fixes.

The earlier 75/52 and saved-workspace 84/61 runs belong to their own frozen
sources. Saved `.crab/plots` template closure was still open at saved-workspace
qualification; the subsequent scoped proof below closes that retention boundary.
The earlier unexplained cold-retrieval timeout remains unresolved.

The declared saved-template correction has eight passing actual CLI cases:
declared resource retention and offline export after source removal, same-name
built-in overrides, over-limit unavailable identity, canonical private Crab/Git
and traversal refusal, recorded-tree execution/reproduction isolation, truthful
saved observations and the existing guarded workspace-action boundaries. The
recovery-copy regression separately passes against the real CLI. It retains the
full attachment diagnostic while the primary notice names the actual byte limit
and new-observation recovery; no runtime fallback, serialization change or new
dependency contract is introduced by this slice.

Final source-bound Darwin-arm64 qualification against the owned RustFS fixture
passes all 89 required capabilities and 65 required cases, plus the separate
10,000-run catalog check. Three packaged native journeys, six browser cases, the
real browser/HTTP/storage journey and current HTTP grant checks pass. The final
source-tree binding is
`f04cc3792c2ee803b60bb8cdb00a595ea9e75ef4595ae6836c7b08f8dac0b2a5`;
renderer runtime remains
`af20a84eb867bbf7357425e37a583672c669568f0435c20f315e111d513fc54e`.
Independent reconciliation reproduces current HEAD/source/runtime and all four
actual CLI/package hashes, matches the pre-run binding, and passes the independent
evidence verifier. Archived and canonical evidence are byte-identical.

All six required 1280/900 × 1000 captures are byte-identical to this final canonical
native run. They show the saved authored title/curve after source deletion,
over-limit capture with 0.75 retained and no stale SVG, and the healthy immutable
report after moving the entire local Results cache away and retrieving remotely.
Results remains the same Data Workflows tab in every capture; the native
comparison test asserts no standalone Primary Results button. The scoped visual
**ship** disposition resolves only the initial recovery-copy finding. Earlier
pending qualification statements and pre-fix 89/65 proof remain historical. The
detailed record is
`.impeccable/review/results-saved-templates/documentation-review.md`.

Explicit pre-capture metadata migration remained open at the saved-template gate;
the subsequent qualification below closes its supported-v1 slice. That historical
proof does not approve whole Results, general DVC parity, whole Plan 020 completion
or release readiness.

The metadata migration extension has three passing actual CLI regressions:
recorded scalars/status and refusal of unsupported execution/dataset claims;
invalid schema/ID, oversized/nonregular and scalar-field-overflow preflight;
and preservation of existing captures with corrupt-root/body refusal. Shared-owner
proof passed 54 tests with three explicit declaration/evidence producers ignored.
Owner-generated declarations/schema, both clients and native-test types, four
evidence-verifier cases and the 399-page/4,308-fragment docs check passed. The
initial regression recorded the missing `migrate` command. A mistaken test
assumption about imported dataset identity was corrected to assert the existing
strict witness refusal; the validator was preserved.

Final canonical qualification reached terminal exit 0 on Darwin-arm64 against
the owned RustFS fixture: all 94 required capabilities and 68 required cases,
plus the separate 10,000-run catalog check. Three packaged native journeys, six
browser cases, the actual browser/HTTP/storage journey and current HTTP grant
checks passed. Source-tree binding is
`7c557170ab22ee13fa42ad88e59ddaa1f9948ce2eeb105f6e44e5030e3d830e3`,
at HEAD `040916ccd056162dca9d2962a8b97843c1a14007`; renderer runtime remains
`af20a84eb867bbf7357425e37a583672c669568f0435c20f315e111d513fc54e`.
Independent reconciliation matches current HEAD/source/runtime and all four actual
CLI/package hashes to the final pre-run binding. Independent verification passes;
archived and canonical evidence are byte-identical.

All eight current captures match the final canonical output byte for byte.
Native working/cold-report captures are 1280/900 × 1000 and retain imported
0.375 despite live metric 999. Browser report/catalog captures use 1280 × 1000
and 390 × 844 viewports; their full-page PNGs are respectively 1280 × 1039,
390 × 1466, 1280 × 1000 and 390 × 1324. The immutable browser report retains
recorded 0.4/0.6, independently of its current catalog. Native Results remains
inside Data Workflows; its regression explicitly asserts no standalone Primary
Results button. Both clients show honest evidence absence and unavailable actions.

The initial review found one recovery contradiction: immutable/browser context
guidance replaced the imported no-capture explanation. Final shared policy and
browser tooltip priority resolve that finding. Assertions cover all four
working/immutable × local/browser contexts and browser tooltips. Reopened images
retain wrapping, internal scrolling, values and disabled execution without toast
obstruction or stale plots. The visual **ship** disposition covers only this
single finding. The detailed record is
`.impeccable/review/results-metadata-migration/documentation-review.md`.

Pre-fix metadata source
`675c0ccdbb92d5e98c268513bdd5a786610f29521c038d6a91cb0badbb17422b`
and its 94/68 evidence/captures remain historical in `pre-fix/`; they do not
qualify the guidance correction. Saved-template source `f04cc379…` and its 89/65
proof likewise remain historical. The earlier unexplained five-second cold
retrieval timeout and failure artifacts remain unresolved. Partial authored-template/
transform variants, actual provider outages/in-flight failures, managed-service
qualification, other OS packages, CI/release proof and remaining Plan 020 gates
stay open. Existing narrow workflow-list compression and internal scrolling are
unchanged, neither canonized nor repaired. This platform/provider qualification
does not approve whole Results, general DVC parity, whole Plan 020 or release.

The subsequent native transport-failure proof extends qualification to 96 required
capabilities / 68 cases plus the separate 10,000-run catalog, on source
`bc20aa4f4bd75f2ddf91e0037d872290c4554cf8b4a66705a3a6dadc9c5ea456`.
The three packaged native journeys, six browser cases and real browser/HTTP/storage
case pass with no skips or flaky results. Independent verification matches current
source/runtime and all four actual artifacts to the pre-run binding; archived and
canonical evidence bytes match. Runtime and all package artifacts are unchanged
from the metadata-migration product; only test coverage and its required evidence
contract changed. No fresh visual ship verdict is claimed.

The owned loopback proxy preserves signed request headers and forwards actual
storage responses. The native journey waits for a real attachment GET/200, then
disconnects every transport connection and refuses the whole endpoint. Its SDK
boundary failure remains visible alongside the same immutable report and both
recorded values; all four curve paths survive. Forwarding restoration and another
attachment selection recover the actual CSV. Closing the native session during a
second held GET releases that response, and returning to the retained local report
shows both runs without late evidence or errors. Neither RPC nor successful
payloads nor product retry policy are replaced. Browser outage behavior is not
qualified by this native test.

The three final 1280/900 × 1000 subject captures were opened and archived
byte-identically with hashes/dimensions in `results-outage-96-proof-summary.json`.
They show the error and Refresh view control, preserved comparison/report identity,
and actual local recovery. Existing narrow workflow-list compression is unchanged.
Native-test types, four verifier cases and diff checks pass. Catalog queries
measured 4.87/4.90 seconds cold/warm for 200 rows / 32 columns / 648,892 bytes;
these are fixture observations, not an SLA. Browser outages, other provider failure
variants, authored-template/transform variants, managed-service qualification,
other native OS packages and CI/release gates remain open. Earlier bindings and
the unexplained retrieval timeout remain historical; whole Plan 020 is incomplete.

The browser storage-loss/read-timeout extension passes final source-bound
Darwin-arm64 qualification against the owned RustFS fixture: all 99 required
capabilities and 69 required cases, plus the separate 10,000-run catalog check.
Three packaged native journeys, six browser cases, the real browser/HTTP/storage
journey and current HTTP grant checks pass with no skipped, unexpected or flaky
results. Source-tree binding is
`a116cbe5c6c0de969c69b6437b6a63edaed944a7352881bc370886abfd3031bf`,
at HEAD `040916ccd056162dca9d2962a8b97843c1a14007`. Independent reconciliation
matches current source, renderer runtime and all four actual native artifacts to
the pre-run binding; independent evidence verification passes. Archived and
canonical evidence bytes match at SHA-256
`defc831ebe05ce534b79a76bbc3f104cd7ce1901d62fdf427355698f176c1e4a`.
The native runtime/artifact hashes are unchanged; neither the CLI nor desktop
agent depends on `crab-server`. Rebuilt server/UI assets exercise the new shared
read-timeout message and browser Refresh view label through real HTTP.

The browser's private keep-alive transport proxy forwards signed requests and
actual owned-storage responses. It observes a retained attachment GET/200 before
disconnecting the whole endpoint, then separately holds an observed response to
reach the shared 504 deadline. Exact allowed 503/504 messages are checked. Both
failures retain the same report ID, 0.1/0.2 observations and four curves; restoring
forwarding and reopening the attachment returns the actual CSV. Existing report
publication, reload, repository isolation and revocation checks also pass. A
separate real GET/POST regression drives the same stalled handler through shared
admission and preserves the mutation warning. The fixture explicitly selects
`object_store` 0.14.1's service-specific `S3Endpoint`, honoring its precedence
over the generic endpoint. No dependency, runtime fallback or product setting is
added.

All eight reviewed full-page PNGs retain their exact scoped-run bytes, hashes
and dimensions: six disconnection/timeout/recovery captures and two later lineage
captures. Wide failure viewports are 1280 × 1000; narrow viewports are 390 × 844.
The subsequent lineage-wide viewport returns to its original 1280 × 720 before
capture. Final canonical capture widths independently match those intended
viewports; the scoped images are not claimed byte-identical to later full-run
images. The scoped real journey passed in 73.4 seconds. The visual **ship**
disposition has no material fixes and covers only browser failure/recovery copy,
action explanation and frozen-state preservation. The detailed record is
`.impeccable/review/results-browser-storage/documentation-review.md`.

This closes the owned-fixture browser transport-loss and held-read slice, without
qualifying general provider outages or other in-flight failure variants. Earlier
endpoint-bypass, viewport-cleanup and 503-only failures remain historical; neither
their passed subsets nor the earlier unexplained cold-retrieval timeout is erased
by this run. Partial authored-template/transform variants, managed services,
other providers/OS packages, CI/release proof and remaining Plan 020 gates stay
open. Existing shell compression and the lineage captures' visible Skip to content
focus link are neither canonized nor repaired. Native Results remains inside
Data Workflows, with no standalone sidebar entry; no new system or raster asset
is introduced. Whole Results, general DVC parity and release are not approved.

The authored runtime-failure correction has completed scoped source-bound proof
on `b7df97c392fb3f23b162c143dbfd123bb37de133d0682f4adf867b5d2d852120`,
with bundled runtime
`fafdbcbc1d546ae7b02fd1328a40fc72e4811317d965016f6d3111d8d89d65fc`.
The actual CLI captures, applies and exports retained authored plots; browser
and packaged Darwin-arm64 native cases pass with zero skipped, unexpected or
flaky results. Initial evaluation, later checkbox-triggered evaluation and syntax
failures remain visible alongside score 0.25, the same report and one healthy
curve. Both HTML exports make no HTTP/network attempts. Reloading/reopening the
report recreates the frozen plots without unhandled page errors.

All six scoped captures were independently opened and their hashes/dimensions
checked: immutable HTML at 1280 × 1705 / 390 × 2093, browser Results at
1280 × 1976 / 390 × 2487 and native Results at 1280/900 × 1000. Native captures
show the internally scrolled failed-chart region; browser/offline document tops
remain visible. The visual **ship** disposition has no material fixes and covers
only authored initial/user-triggered failures, failure isolation and retained
truth. Results remains Data Workflows → Results, with no standalone native entry.
Existing syntax wording, narrow shell compression and the browser focus link are
unchanged. The detailed record is
`.impeccable/review/results-authored-failures/documentation-review.md`.

Expanded source-bound Darwin-arm64 qualification against the owned RustFS
fixture passes all 102 required capabilities and 72 required cases, plus the
separate 10,000-run catalog check. Four packaged native journeys, seven browser
cases, the actual browser/HTTP/storage journey and current HTTP grant checks
pass with no skipped, unexpected or flaky results. Independent reconciliation
matches current HEAD/source/runtime and all four actual CLI/package artifacts to
the pre-run binding. Independent evidence verification passes; archived and
canonical evidence bytes match at SHA-256
`969acadccf7cfa0424ece62e8249be759cff5563fa0cf0b893c5c6f9241ce536`.
This full proof supersedes pending qualification language for the same authored
failure source. It does not widen the scoped visual verdict. The six scoped PNGs
remain byte-identical to their original scoped output; final canonical authored
capture widths independently match 1280/390/1280/390/1280/900 without claiming
identical pixels or generated record identities across the two runs.

The earlier 99/69 source and its runtime/package evidence remain historical.
Normal client types, the focused new native-test type probe and UI/native package
builds pass. The additional broad desktop `tsconfig.tests.json` check fails on
existing missing-JSX and unrelated test diagnostics; its configuration is
byte-identical to HEAD and is not reported as passing. No new system or raster
asset applies.
Other authored-template/transform variants, provider/managed-service access,
other native OS packages, the unexplained cold-retrieval timeout and CI/release
gates remain open. This platform/provider proof does not approve whole Results,
general DVC parity, whole Plan 020 or release.

The partial retained-template correction has completed scoped proof on source
`644b8eebaaf08f6b2317a579a43b700f903910e2ceb6510273d01c87974bc0ec`,
at HEAD `040916ccd056162dca9d2962a8b97843c1a14007`. Bundled runtime remains
`fafdbcbc1d546ae7b02fd1328a40fc72e4811317d965016f6d3111d8d89d65fc`.
The shared chart owner now admits each revision's evidence before selecting its
template/counting policy. A regression covers missing, uncaptured, corrupt and
denied templates in both selection orders, exact surviving values and sanitized
reasons. Existing 12 chart, nine template and five matrix cases pass. Admitted
template mismatches, non-template declarations, parsing and statistical-sampling
refusals remain strict. No reader, storage, model, parser, renderer, wire or
dependency change is introduced by this correction.

The actual CLI saves observations 0.1/0.2, records an over-limit custom template
as explicitly unavailable, publishes the partial report, clears local Results
state and removes source YAML/template, then pulls and exports the same report
through the owned RustFS fixture. The cold catalog has no run roots. Only the
verified revision's loss values 1/2 enter the chart; the unavailable revision's
3/4 never do. The CLI regression and authored-failure sibling pass. Browser and
packaged Darwin-arm64 native cases each pass with one expected case and zero
skipped, unexpected or flaky results, in 9.05s and 8.15s. Browser consumes the
actual report through controlled API replies; native uses normal local-session
reads of the actual cold report. Offline HTTP/network attempts remain zero.

All six scoped PNGs were independently opened and their signatures, dimensions
and hashes match the manifest: immutable HTML at 1280 × 1379 / 390 × 2012,
browser Results at 1280 × 1546 / 390 × 2077, and native Results at 1280/900 ×
1000. Browser/offline document tops remain visible; native captures intentionally
show the internally scrolled notice and healthy curve. The scoped visual
**ship** disposition has no material fixes and covers classified partial-template
availability, retained truth and notice wrapping only. Native Results remains
Data Workflows → Results with zero workflows and no standalone native entry.
The detailed record is
`.impeccable/review/results-partial-templates/documentation-review.md`.

Scoped package build, UI/focused-native types, workflow production-library
Clippy with warnings denied, format, focused Prettier, runtime reproducibility
and diff checks pass.

Expanded source-bound Darwin-arm64 qualification against the owned RustFS fixture
now passes all 105 required capabilities and 74 required cases, plus the separate
10,000-run catalog check. Four packaged native journeys, seven browser cases,
the real browser/HTTP/storage journey and current HTTP grant checks pass, with
zero skipped, unexpected or flaky results. Independent reconciliation matches
current HEAD/source/runtime and all four actual CLI/package artifacts to the
pre-run binding. Independent evidence verification passes; archived and canonical
evidence bytes match at SHA-256
`9e2814cb3aee7a021607ad2332feaacdee5df8c9d3161e4a6bbbf19704e46b8f`.
This full proof supersedes pending language for this same binding without
widening the scoped visual verdict. All six scoped PNGs retain their original
scoped bytes; final canonical partial-template capture widths independently
match 1280/390/1280/390/1280/900, without a claim of identical pixels or generated
record identities across runs.

The earlier 102/72 source and artifacts qualify only their own binding. Broad
desktop test-project types remain a previously recorded failure, not a fresh
pass. Other authored-template/transform variants, provider/managed-service and
other OS qualification, the unexplained cold-retrieval timeout, CI/release proof
and remaining Plan 020 gates stay open. Existing shell compression and focus-link
drift are neither canonized nor repaired. No new system or raster asset applies;
this platform/provider proof does not approve whole Results, DVC parity, whole
Plan 020 or release.

## Completion audit and regression gates

The preceding regression-gate Darwin-arm64/RustFS acceptance passed 105 capabilities /
74 required cases, plus the separate 10k catalog, at source tree
`3ecbbc32a93a7cc389432ba933571f79f49063abf6eb273538409794d8ebb708`.
Current HEAD is `040916ccd05`; runtime and all four actual executable/package
digests match the pre-run binding. Independent verification checks every native
(4), browser (7) and actual HTTP/browser (1) case has one passed attempt, with no
skips, unexpected/flaky results or report errors. Evidence SHA-256 is
`5a75d6dd55f77c5b2feebc0f65ae62e61fa3c9284e75d7d4653139e5298ba642`;
the full root/browser artifacts are retained in
`results-completion-audit/accepted/` under this worktree's external target.

The audit also passes all 36 existing experiment, 55 Results owner, 39 metrics/
plot and 22 non-S3 CLI Results regressions, generated client equality, CLI schema
drift/Results validation and minimal/Results SDK builds. The SDK dependency gate
now checks 14 cross-target profiles, including four Results combinations, and
requires the workflow owner only behind the opt-in feature. Native workflow CI
now runs the existing CLI/plot regressions and separate schema/SDK gates; actual
hosted execution remains unverified. Browser formatting/build, both product type
checks, 36 preference/session tests, focused Results lint, Rust formatting,
runtime/license reproducibility, evidence-verifier tests and web docs checks pass.

Results remains under Data Workflows → Results. This audit changes qualification
and browser formatting, with no native UI, runtime, stored/wire model, dependency
version, manifest or lockfile change. It does not create a new visual verdict.

The completion matrix in Plan 020 identifies the remaining evidence precisely.
The earlier browser lifecycle case measured post-GC renderer heap across ten switches;
its binding did not include native/server reader and OS-handle envelopes. The desktop
lint command cannot start without its pre-existing missing ESLint configuration;
broad test-project types remain a recorded failure distinct from passing product
types. Other provider/managed-service and native OS packages, actual CI/release
producer results and the historical five-second retrieval timeout remain open.
Intentional parse/template/sampling refusals are documented behavior, not an
unnamed backlog of authored-plot variants. Whole Plan 020 remains in progress.

## Reader and descriptor lifecycle qualification

Qualification now observes real response lifetimes alongside per-process file
descriptors and post-GC renderer heap. The native journey repeatedly unmounts
Results through the workflow tabs and reloads the renderer, then compares its
actual retained remote report. The HTTP journey switches repositories, reloads
and reopens a real published report through authenticated routes. Each warms five
cycles and retains a baseline plus twenty measured cycles. Native measurements
cover the packaged main/sidecar processes and physical proxy reads; server
measurements cover its actual test process and existing storage-observer reads,
including response-stream termination. They do not add a product service/API.

Every settled sample requires zero active reads, matched start/finish counts and
new read observations. The verifier independently enforces an eight-descriptor
increase ceiling per measured process and sixteen MiB of renderer heap growth
from the warmed baseline. Native cancellation/disconnection must leave an
additional balanced terminal read observation. Missing measurements cannot be
replaced by a capability flag or a previous run's artifact. These are explicit
fixture envelopes, not configurable product limits or an unbounded leak claim.

Both scoped Darwin-arm64/RustFS journeys pass at source tree
`b5f50facaabeab1fafea59686f19fde6aeef205fa927910673c7bf7ca8713465`:
native descriptors remain 104/12, server descriptors return from a maximum 73 to
their baseline 72, and renderer heaps finish below their warmed baselines. Raw
samples and source/artifact reconciliation are retained under
`results-resource-envelope/`. The expanded fresh 107-check acceptance now passes;
the earlier 105-check source remains qualified only for its own binding.
The original stale-handle fixture failure is preserved separately; the existing
native session guard remains unchanged. No production UI, runtime, persisted
contract, dependency or packaging input changed. Other platform/provider and
CI/release qualification, existing desktop check failures and the historical
retrieval timeout remain open; whole Plan 020 remains in progress.

Final full qualification at the same source passes **107 capabilities / 74
required cases**, plus the separate 10k catalog. Independent reconciliation
matches current HEAD/source/runtime and all four actual CLI/package artifacts to
the pre-run binding. All four native, seven browser and one actual HTTP/browser
case have exactly one passed attempt, with zero skipped, unexpected or flaky
results and no report errors. Raw resource envelopes equal the embedded evidence
and pass independent verification. Canonical and retained evidence bytes match
at SHA-256 `ad012d1a3689328d4744e613ad82ca8567e465947556efb3f2eddd357b30635c`.

In the full native trace main/sidecar descriptors return to 104/12 after a sidecar
peak of 13; renderer heap finishes 4,602,972 bytes below baseline. All 341 observed
reads finish by the final measured sample, and all 366 finish after subsequent
disconnection/session-cancellation cases. The full server trace returns to 71
descriptors after a peak of 72; its renderer heap finishes 277,116 bytes below
baseline and all 1,971 reads finish. Each trace contains a baseline and twenty
measured cycles after five warmups. These results close the repeated native/
server reader, descriptor and renderer-heap measurement gap for this qualified
scope. They do not qualify another platform/provider or establish a retrieval SLA.

The 10k fixture returns 200 rows / 32 columns / 648,892 bytes, with build/cold/warm
times 4.39/4.88/4.86 seconds. Maximum-overlay render/zoom/cursor measured
165.7/42.8/7.3 milliseconds. Final artifacts and nonsensitive host metadata are
retained in `results-resource-envelope/accepted/`; the source binding and
`full-proof-summary.json` identify their scope. Existing desktop check failures,
other provider/managed-service and OS qualification, actual hosted CI/release
proof and the historical retrieval timeout remain open. Results remains inside
Data Workflows → Results; whole Plan 020 remains in progress.

## Installed CLI offline qualification

The actual default `make -C crab install` now passes on Darwin arm64 at source
tree `b5f50facaabeab1fafea59686f19fde6aeef205fa927910673c7bf7ca8713465`.
It builds both Make release variants and the cache server, then stages through
the canonical installer with an isolated `PREFIX` and an absent `CARGO_BIN`
mirror destination. Expected helper symlinks and binary digests are verified;
normal user installation paths are untouched. The installed CLI digest is
`d086b305b722e5d7d1217a7ab1df48504442775aed118ad3fb8b385e9346b1fa`.

With no Node/npm on its PATH, the installed CLI executes two real runs, records
0.1/0.2 scores and a -2.5 loss delta, creates a report and exports both Results
and legacy metrics HTML. Report export remains byte-identical after eleven
source/workspace files, including the authored template, are removed. The frozen
report reopens unchanged. In an offline Chrome context, Results renders two
charts/four series/two embedded images and linked Zoom; metrics renders two
charts/two series/one image. Authored titles render, with zero network attempts,
page errors or console errors. Both screenshots were inspected.

The separate `results-installed-cli/` packet under this worktree's external target
retains actual commands, terminal results, scripts, HTML, DOM observations,
screenshots and an independently reconciled `proof-summary.json`. The earlier
107-check source/runtime and four CLI/package artifacts remain unchanged; that
full evidence is independently re-verified. This qualifies default Make
installation and standalone offline exports on the measured platform. Native
release producers, other OS/providers and hosted CI remain separate open gates;
the plan remains in progress. Native Results remains a Data Workflows tab.

## Native CLI archive qualification

The unmodified native release producer now completes both Darwin-arm64 builds
and creates the actual CLI/FUSE/NFS archive at the same bound source tree
`b5f50facaabeab1fafea59686f19fde6aeef205fa927910673c7bf7ca8713465`.
Archive SHA-256 is
`8ac28aabc0734cf18a3041a5213f6043aadd3f43e560d0d882c03b67de043e62`;
the extracted CLI is
`001fee12a146fe273db8f66861055ffdbe89957b7a880c51cf7bd4f1c8318950`.
Checksums, exact archive members, helper symlink, arm64 architecture, version and
main-CLI FUSE independence are verified. No feature override or publishing step
was used.

The extracted CLI executes two real runs without Node/npm, captures 0.1/0.2
scores and a -2.5 loss delta, applies the first run and exports both HTML forms.
The frozen report remains identical after eleven source/workspace files are
removed. Independent CLI reads match both retained runs and the report. Offline
browser proof checks actual metric previews, curve/image counts, linked zoom
and cursor rules, and the authored metrics tooltip. Both exports contain the
exact bundled runtime and make no network attempts or browser errors. Screenshots
were inspected. `results-release-cli/` under the external target retains the
archive, actual commands/results, scripts and independent reconciliation.

The existing source/runtime and four full-acceptance artifacts remain unchanged;
107-check evidence is independently re-verified. Other release targets, actual
hosted CI, provider/managed-service qualification, existing desktop check failures
and the unexplained historical retrieval timeout remain open. Current Results
CI wiring selects macOS arm64 and Linux, so it does not qualify Windows or Intel
macOS Results. Native Results remains inside Data Workflows; the plan stays in
progress.

## Portable CLI and CI binding qualification

CLI Results fixtures now use one resolved absolute Python interpreter and typed
argv across platforms, replacing the suite-wide Unix gate and hard-coded Unix
programs. Native symlink APIs are selected per platform; Unix mode assertions
remain intact and directory-byte/membership tests are selected on Windows. Native
workflow CI provisions Python 3.12 and selects Results regressions on all three
runners. Windows execution remains unverified until an actual runner completes.

Qualification now binds `workflow-native.yml` as well as `desktop.yml`. A real
Git regression caught the omitted native CI input before the fix; all six
source/evidence tests now pass. All 22 local CLI regressions pass, and fresh full
Darwin-arm64/RustFS qualification passes 107 capabilities / 74 required cases
plus the separate 10k catalog at source tree
`b3c837c5f5cebb79c2c1dc59845807d2f9ee1abc2c5e89854efe8fd875a162a4`.
Independent reconciliation verifies all 3,739 source-file identities, current
HEAD/runtime and four actual CLI/package artifacts, with exactly one passing
attempt for each native/browser case and no skipped/unexpected/flaky outcomes.
Evidence SHA-256 is
`c85f057e6663b8db86284a5823cde237374f060bdc1d7fd99fb4724a7dbe1985`.

Both twenty-cycle resource envelopes pass: all native reads finish, including
later cancellation, and all 1,975 HTTP reads finish; renderer heaps remain below
their warmed baselines. Raw artifacts, source closure, CI selection and scoped/
full results are retained in `results-portable-cli/` under the external target.
Runtime/package bytes, product contracts, dependencies and design guidance are
unchanged. The local Windows MSVC test-target check passes with the advertised
SDK `10.0.22621`, after the default SDK setup failed on missing x86_64
`Win11SDK_10.0.26100` headers. Both command receipts are retained; the passing
`windows-proof-summary.json` binds the compile check to the same source tree.
This does not execute Windows tests. Native Windows runtime, other
platform/provider and hosted CI/release proof, existing desktop check failures
and the historical retrieval timeout remain open. Results stays inside Data
Workflows; the plan remains in progress.

## Portable checkpoint acceptance

The real checkpoint/apply/reset/resume fixture now uses typed Python argv and a
tracked script on both platforms, retaining lineage and failed-reset rollback
checks. Resume additionally parses the complete JSON result and matches its new
run ID. Windows ACL setup redirects helper stdout to stderr so progress cannot
precede the structured CLI result and diagnostics remain available.

At source tree `d75a7e2a4abe40925c746ea62af06e8741e38d4a7365a146884bfe50a2bc3fc7`,
the actual macOS journey and Windows MSVC experiment/Results test-target checks
pass. Native CI now selects this exact checkpoint case on Windows; hosted
execution and native Windows runtime remain unverified. Source, commands, logs
and parsed CI proof are retained in `results-checkpoint-diagnostics/` under the
external target. Rust format and diff checks pass.

The preceding 107-capability packet belongs to its recorded earlier source.
The current native package build and full acceptance pass: 107 capabilities /
74 required cases, plus the separate 10k catalog. Independent verification
matches all 3,739 source identities and four actual CLI/package artifacts to
the pre-run binding. Each of four native, seven browser and one HTTP/browser
cases has one passed attempt, with no skipped/unexpected/flaky results.
Evidence SHA-256:
`73611112754ddb0576c18dbc6e5893a56f5927564e6dc8a85c0c2148e3099f41`.
Both twenty-cycle resource envelopes pass; all measured readers finish and
native cancellation leaves no active readers. Only the native CI producer,
checkpoint helper and fixture changed; Results models, dependencies, UI and
runtime assets remain unchanged. Windows
packaged process/handle observation and producer paths, other platform/provider,
hosted CI/release, existing desktop check failures and the historical retrieval
timeout remain open. Results remains inside Data Workflows; Plan 020 is in progress.

## Windows native qualification harness

Desktop CI selects the complete native Results profile on Windows: 77 core,
fault and desktop capabilities / 64 cases, plus the 10k catalog. macOS arm64 and
Linux keep the complete HTTP/browser profile. Package binding now selects the
host platform/architecture and native executable names, and refuses foreign
packages. The producer shares the existing short Windows RustFS layout and
Python AWS CLI owner and invokes Playwright through Node entrypoints.

The native lifetime observer measures real Windows OS handles and binds the
sidecar to its parent PID and canonical executable. Every sample checks that
actor identity. Hidden, noninteractive PowerShell commands use Unicode-safe
encoding and reject invalid measurements; no budget or failure assertion was
relaxed. Windows compilation passes for 101 test targets, including the library
tests. Eight harness/evidence tests, all 22 regular CLI regressions and focused
native-test types pass. Windows query-boundary checks are mocked; actual native
Windows execution remains unverified until CI runs.

Source `859b9b13…` and the unchanged compiled artifacts are bound in
`results-windows-harness-console/` under the external target. Full acceptance
and independent verification pass: 107 capabilities / 74 required cases, plus
the separate 10k catalog. All twelve native/browser/HTTP-browser cases pass on
their only attempt, with no skipped, unexpected or flaky results. The verifier
matches 3,739 source identities, the runtime and four actual CLI/app artifacts;
evidence SHA-256 is
`e7e8220079c2d3ca70bcc515f93a8fe7fbb8edcae4a8a0450711389663cd9a90`.
Both twenty-cycle resource envelopes pass, with no active readers after native
cancellation or HTTP completion. The packaged native journey queues two real
runs, compares metrics/plots, restarts and retrieves the comparison remotely.
It enters Data Workflows → Results and explicitly rejects a standalone Results
activity-bar button, including on a bare remote without workflow YAML.

The preceding full packet is retained separately. The pinned RustFS
release has no Intel Mac binary, so that fixture gate remains open alongside
provider, hosted CI/release and existing baseline check gaps. Results remains a
Data Workflows tab; the whole plan remains in progress.

## Live-provider Results transport gates

Credentialed SDK jobs now select Results-specific S3, GCS and Azure cases. Each
uses the existing dedicated provider fixture and workflow publication owner;
it retries publication, removes source/local state, verifies cold retrieval and
recovers the immutable report after deleting run discovery. A fresh SDK client
reads the report and exact blob bytes without changing the repository inventory.
Cleanup is limited to the test's unique repository prefix. The shared fixture
is one sealed scalar/CSV observation, separate from packaged two-real-run proof.

The Results-only SDK test and owned-RustFS S3 case pass. Actual AWS/GCS/Azure and
managed-service qualification remain unverified. Parsed YAML and stubbed Bash
execution confirm all three exact CI selections; this is wiring proof only.
Source binding now includes the SDK workflow alongside desktop/native workflow
producers. Source `abe0a9f5…`, actual artifacts and scoped receipts are retained in
`results-provider-contracts/` under the external target; refreshed full acceptance
and independent verification pass: 107 capabilities / 74 required cases, plus
the separate 10k catalog. All twelve native/browser/HTTP-browser cases pass on
their only attempt, with no skipped, unexpected or flaky results. The verifier
matches all 3,740 source identities, including the SDK workflow, and the runtime
and four actual CLI/app artifacts. Evidence SHA-256:
`74cac8fe96f2ef6b480d61102484455205ccee921d5ea9896f7f00f4096b713b`.
Both twenty-cycle reader/descriptor/heap envelopes pass; native cancellation and
HTTP completion leave no active readers. Runtime/product sources, contracts,
dependencies and native navigation are unchanged: Results remains under Data
Workflows. Provider/platform and hosted CI/release gates remain open.

## Results schema discovery

`crab version --json` now advertises `workflow.results: 1.0`, matching actual
Results envelopes and the documented automation preflight. The real CLI
regression fails on the missing entry before the fix and passes afterward;
all 23 regular Results cases and six version tests pass. Qualification requires
that executed discovery check and rejects absent/zero-case proof: 108 full
capabilities / 75 cases, or 78 / 65 for the native-only profile, plus the separate
10k catalog. Nine evidence tests, SDK reads, the owned-RustFS transfer and web
documentation checks pass. No stored/wire model version changes.

The preceding integration run passes its producer, but the external pre-freeze
wrapper used a plain-build CLI that Cargo's integration-test build replaced.
Its rejected binding remains retained. The dedicated pipeline now uses the
canonical test target before freezing the executable digest. Current source
`7a7912b0…` and scoped receipts are retained in `results-schema-discovery/`; package
and full acceptance are in progress. The branch includes `origin/crab-v2` at
`464d943a045`. Browser navigation requires the Results URL; an unrelated SQL
focus assertion reproduces on clean upstream and remains unchanged. Native
Results stays inside Data Workflows, without a separate sidebar entry. The whole
plan remains in progress.

## Idle resource sampling

The schema-discovery package build passes, but its full packet fails when a
late bucket-list read overlaps native heap/descriptor sampling. A focused
trace reproduces the race; failed evidence remains retained. Native process
commands now run asynchronously so the fixture proxy can complete response lifetimes.
Both native and HTTP checks require zero readers and unchanged read counters
throughout measurement, with the existing bounded assertion poll. Five warmups,
twenty measured cycles and heap/descriptor budgets remain unchanged. Resource
assertions are evaluated after sampling; continuous activity still fails.

Only the two lifecycle tests change from the preceding official package build;
runtime, CLI, agent and application bytes remain unchanged. Current source
`8ca8af99…` and pre-run identities are retained in `results-idle-sampling/` under
the external target. Focused types, formatting, qualification tests and Windows
query-boundary checks pass. Two packaged repetitions complete all 21 idle
samples with unchanged descriptor counts. A third reaches the 360-second test
timeout and worker teardown timeout without producing a lifecycle sample;
the blocked operation is not established. Three traced full journeys and ten
shortened first-close checks subsequently pass, without resolving that timeout.
Failed evidence remains separate from API/process tracing in
`results-close-diagnostics/` and `results-close-minimized/`.

The refreshed full producer then fails during HTTP fixture startup because the
volume has less than its required 3 GiB free (2 GiB Git receive reservation plus
1 GiB reserve). Reclaim only this checkout's unused cross/release compiler
caches, keeping bound artifacts and installed binaries unchanged. The exact
real-storage HTTP test passes after reclamation. The fresh producer and
independent verifier pass under `results-capacity-recovered/`: 108 required
capabilities / 75 required cases plus the separate 10,000-run catalog. All twelve
native/browser/HTTP-browser cases pass once, without skipped/flaky/unexpected
results. Source `8ca8af99…`, 3,741 source identities, runtime and four artifacts
match their pre-run binding. Evidence SHA-256:
`c99e85930d4e2209fe6b918eee6d13675ff00059f4a0b7145461f6e54faa417b`.
Native handles remain 104/12; HTTP handles move 71→75 within the existing budget.
Both twenty-cycle envelopes end with zero active readers and post-GC heap within
the warmed baseline. Product limits remain unchanged. The passing packet
explicitly leaves whole-plan completion false. Actual cloud/managed and other native platform execution,
the prior native timeout and whole-plan completion remain open. Results remains
a Data Workflows tab, with no native sidebar entry.

## Compound visualization example and fractional persistence

`packages/web/public/examples/ml-results-lab/` provides a reproducible Python
standard-library workflow: four real queued configurations, classification and
forecasting, separate training/validation/test splits, and two model artifacts.
Five plot groups contain learning overlays; precision/recall, ROC and calibration;
cohort Wilson intervals; a threshold/cohort/brush explorer with exact confusion
counts; and forecasts, validation-calibrated residual bands and residuals.
The benchmark input manifest hashes both datasets and evaluator source. The
example driver also checks their recorded input witnesses before declaring
comparability and assigning metric objectives.

The canonical parser retains every observation column internally. Authored
Vega-Lite templates can reference auxiliary fields without selecting them as y
series and multiplying observations. JSON/YAML preserve auxiliary types;
CSV/TSV normalize numbers and empty cells. Metadata and coordinate aliases
reserve all source-column names. Cross-file coordinates replace only declared
x values. Tagged CLI string rows, schema, storage layout, offline URL rejection
and display budgets stay unchanged. Compatible simple curves retain shared
cursor/range behavior; compound templates own their authored interactions.

A real four-run comparison exposed fractional deltas failing exact report
verification after JSON reload. `crab-workflow` enables serde_json 1.0.151's
`float_roundtrip` feature, keeping exact verification rather than adding float
tolerances. Dependency source `src/de.rs::f64_from_parts` selects its concise
float parser under this feature. No dependency version, lockfile, override or
vendor change. The minimal save/reopen regression fails before the feature and
passes afterward; the originally failing report also reopens unchanged.

Focused proof: 51 Results-owner tests pass (two dedicated scale tests remain
ignored), CLI build and formatting pass, and documentation links pass. The
actual example produces four successful runs, five unsampled chart groups and
3,632 observations. Browser proof exercises threshold changes, cohort filtering
(532 observations), brushing (432), test-only forecast filtering (84), learning
zoom and reopening; zero network attempts, page errors or console errors. Nine
screenshots retain those real rendered states.

Owned loopback RustFS proof publishes the runs/report, removes all local Results
and working templates, retrieves the report and all four runs, then rebuilds
five healthy charts. Offline HTML SHA-256 is byte-identical before and after:
`a4d15a2aeeb8553c85f92724bce2349751a3f1bf8948060926a7252224d97739`.
Report: `2e5164ef4d9efcd921c1936dabe2c5d5e49d5e64376914253a118dee23d19c13`.
Evidence is retained in this worktree's external target under `results-lab-proof/`
and the local screenshot gallery's `complex/` directory. Browser brush targeting
and the fixture Python environment were corrected in the external harness;
production timing/assertions were not relaxed. This is scoped CLI/shared-owner/
offline-browser evidence, not a new native package, cross-platform, live-cloud
or whole-plan qualification. The earlier native timeout remains unresolved.

## Native compound visualization qualification

`packages/desktop/tests/e2e/results-complex.spec.ts` runs the public lab in the
packaged application under **Data Workflows → Results**. Native enqueue admits
four two-parameter configurations; the visible Queue Start action executes them
with the bundled CLI. The case binds the captured benchmark witness, objectives
and baseline, verifies all five chart groups, and drives threshold/cohort/brush,
forecast-period and curve-range controls. It reopens the same profile, verifies
four selected runs and the epoch range, saves and publishes a report, then
retrieves it from a fresh Git clone after removing the workflow and templates.
The recovered chart still responds to threshold and cohort changes. Successful
evidence is written only after application and owned daemon cleanup completes.

The native two-column chart grid clipped authored compound views. Results now
stacks plots at the available width, matching browser/offline layout ownership;
the shared renderer still preserves authored facet/concatenation geometry and
allows horizontal scrolling when a view exceeds its container. The native case
asserts that these four compound examples fit a 1600-pixel viewport. Example
axis guides inherit the theme, with series colors legible on light and dark
surfaces. Data transformations and frozen-report semantics are unchanged.

The first complete packaged macOS arm64 run passed in 67.5 seconds, capturing
14 screenshots, four runs, five charts, 432 brushed observations, 84 test-period
forecast points and zero page errors. Its immutable report is
`b72246458cf583858741690fe8801c48674b2747d6039df1e4b85dc05da80f6f`.
A final-source rerun passes both the complex case (26.7 seconds) and the existing
Results recovery/failure case (55.9 seconds); all six closes finish below
350 milliseconds. Its report is
`4061187a141f08d81b9608bc97aa0ed844bb93be949220e560027240e2f66738`.
Qualification now requires five native cases and binds the consumed example's
Python, YAML and template sources. This is real native/owned-RustFS proof,
not live-cloud or cross-platform qualification.

## Native timeout investigation

The retained historical failure reached 360 seconds with a further worker
teardown timeout. Its only useful screenshot precedes the first restart; it
has no operation stack or process sample identifying the blocked boundary.
New phase records distinguish launch connection, first window, JavaScript exit,
child exit and stdio closure. They do not change production shutdown policy or
increase acceptance timeouts.

Thirty real comparison/close reproductions, six complete restart/cold-retrieval
journeys (three without added phase instrumentation), and twenty minimal
Electron controls passed. These controls used the earlier packaged application;
they do not establish the new compound-feature acceptance. Some real closes
were slow: JavaScript cleanup completed within a fraction of a second, followed
by a much longer native process wait. Matching Electron 45.2.3 symbols identify
`BrowserProcessIOThread::ProcessHostCleanUp` → `WaitForExitWithTimeout` in Chromium's
network-service shutdown. Electron's DEPS pins Chromium 156.0.8078.80; that source
bounds this wait by build configuration. This explains observed short stalls,
not the historical six-minute failure.

The complex native acceptance also completes all three closes. There is no
reproduced six-minute failure, proven root cause, or justified production fix.
The historical timeout remains open. Do not interpret passing repetitions as a
fix, introduce force-quit timers, or bypass native owner draining. Evidence stays
in the external target under `native-timeout-audit/`, `native-timeout-full-audit/`,
`native-timeout-untraced/` and `native-complex-results/`.

## Desktop control and layout consistency

Results composes the desktop's existing shadcn/Radix Select, Checkbox,
InputGroup, Badge, Empty and Dialog components. Report, baseline and objective
choosers follow the same grouped options and empty-value namespacing as other
workflow controls. Their persisted values and workflow-owned contracts are
unchanged. Authored Vega controls remain owned by their retained specifications.

The toolbar and scrollable content share responsive 16/24-pixel gutters.
Catalog rows separate experiment names from IDs; the comparison's wider pinned
run column uses the same hierarchy and keeps the original accessible name.
Table headers, objective controls, charts, dataset fields and evidence previews
use the existing spacing scale and compact 32-pixel controls. Empty previews
and immutable/baseline labels use shared component composition. Charts retain
full available width and their own horizontal overflow.

A loaded report exposes its complete ID in a named group. During reader failure,
the chooser retains the selected report while the unavailable body is cleared.
Native tests inspect the selected Radix option's complete ID in that state;
they do not require stale report content to remain visible. Tests keep actual
Apply, reproduction, checkpoint resume, artifact CAS, missing/corrupt evidence,
restart and cold retrieval checks, and add keyboard open/Escape/focus recovery
for the baseline chooser.

The shared chart renderer also excludes Vega-Lite 5.23's generated interval
brush rectangles from the accessibility tree, alongside its existing Voronoi
hit regions. The compiler's `src/compile/selection/interval.ts` emits data-free,
clipped rectangles named with `_brush` / `_brush_bg` suffixes. The filter applies
to compiled Vega-Lite gesture layers; authored raw-Vega marks remain exposed.
The example's threshold rule carries an expression-backed description announcing
the current threshold. Top-level template descriptions name the compound views.
The native complex case scans WCAG A/AA with axe after verifying that its scoped
Results surface contains no frames. A browser renderer probe additionally checks
exact counts, the changing threshold label and an authored raw-Vega mark with a
brush-like name, with networking denied.

Final scoped proof: 51 Results-owner tests pass (two dedicated cases remain
ignored); fresh owner-generated browser fixtures pass all four renderer cases.
The final packaged macOS arm64 application passes all five Results acceptance
cases in 212.5 seconds. A further actual four-run journey checks light, dark,
high-contrast light and high-contrast dark at 900, 1280 and 1600 pixels: all
12 combinations have zero scoped WCAG A/AA violations, 24-pixel content gutters
and no workspace-level horizontal overflow. Tables and authored charts retain
local scrolling. Twenty additional theme/menu captures join the 14 feature
screenshots; restart and cold recovery still pass. The original native timeout
remains a separate open investigation; shutdown policy is unchanged.
