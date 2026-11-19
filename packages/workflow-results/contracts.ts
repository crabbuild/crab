// Generated from crab-workflow Results contracts; do not edit.

// prettier-ignore
export type JsonValue = number | string | boolean | Array<JsonValue> | { [key in string]: JsonValue } | null;

// prettier-ignore
export type Availability = "present" | "missing" | "invalid" | "not_captured" | "corrupt" | "denied" | "unsupported";

// prettier-ignore
export type Datum = { file: string, pointer: string, state: Availability, value: JsonValue, reason: string | null, };

// prettier-ignore
export type AttachmentKind = "metric" | "plot" | "image" | "log" | "workflow" | "input" | "output" | "checkpoint" | "template";

// prettier-ignore
export type Attachment = { path: string, kind: AttachmentKind, stage: string | null, hash: string | null, size: number, state: Availability, reason: string | null, push: boolean, remote: string | null, };

// prettier-ignore
export type InputWitness = {
/**
 * Repository dependency key, absent for external or absolute locations.
 */
path: string | null, hash: string, };

// prettier-ignore
export type OutputWitness = { path: string, hash: string, size: number, };

// prettier-ignore
export type StageResult = { name: string, hash: string | null, state: string,
/**
 * Observed execution duration, absent when this attempt did not record one.
 */
duration_ms: number | null, attempts: number, source: string, inputs: Array<InputWitness>, outputs: Array<OutputWitness>, };

// prettier-ignore
export type RuntimeWitness = { program: string, hash: string, };

// prettier-ignore
export type ReproductionSpec = { source_commit: string,
/**
 * Exact source result when this run was reproduced.
 */
source_result: RunSelection | null, files: Array<Attachment>,
/**
 * Closed input trees, including empty directories, restored before files.
 */
directories: Array<string>, runtimes: Array<RuntimeWitness>, targets: Array<string>, recursive: boolean, single_item: boolean, downstream: boolean, pipeline: boolean, all_pipelines: boolean, glob: boolean, unavailable_reasons: Array<string>, };

// prettier-ignore
export type PlotConfig = {
/**
 * Optional DVC plot ID used to target a named plot whose data lives in `path`.
 */
id: string | null,
/**
 * Path to the metrics data file.
 */
path: string,
/**
 * Column or field name for the x-axis.
 */
x: string | null,
/**
 * Optional source file for x-axis values when DVC maps x and y to different files.
 */
x_path: string | null,
/**
 * Column or field names for the y-axis.
 */
y: Array<string>,
/**
 * CSV/TSV inputs do not contain a header row; columns are addressed by zero-based index.
 */
no_header: boolean,
/**
 * Human-readable plot title.
 */
title: string | null,
/**
 * Human-readable x-axis label.
 */
x_label: string | null,
/**
 * Human-readable y-axis label.
 */
y_label: string | null,
/**
 * Plot template.
 */
template: string | null, };

// prettier-ignore
export type ResultOrigin = "captured" | "experiment_metadata";

// prettier-ignore
export type ResultSnapshot = { schema_version: number, origin: ResultOrigin, experiment_id: string, source_metadata_hash: string, base_commit: string, name: string | null, status: string, started_at: string, ended_at: string | null, parameters: Array<Datum>, metrics: Array<Datum>, stages: Array<StageResult>, attachments: Array<Attachment>, plots: Array<PlotConfig>, reproduction: ReproductionSpec | null, capture_issues: Array<string>, };

// prettier-ignore
export type SealedResult = { digest: string, snapshot: ResultSnapshot, };

// prettier-ignore
export type Direction = "minimize" | "maximize";

// prettier-ignore
export type DatasetIdentity = { hash: string, split: string, evaluator: string, };

// prettier-ignore
export type AxisRange = { lower: number, upper: number, };

// prettier-ignore
export type ChartInteraction = { key: string, field: string, label: string, domain: AxisRange | null, };

// prettier-ignore
export type ComparisonSpec = { baseline: string, columns: Array<string> | null, objectives: { [key in string]: Direction }, datasets: { [key in string]: DatasetIdentity }, include_logs: boolean, axes: { [key in string]: AxisRange }, };

// prettier-ignore
export type ComparisonCell = { key: string, candidate: Datum | null, baseline: Datum | null, absolute_delta: number | null, relative_delta: number | null, improvement: boolean | null, reason: string | null, };

// prettier-ignore
export type ComparisonRow = { run: SealedResult, comparable: boolean, cells: Array<ComparisonCell>, };

// prettier-ignore
export type Comparison = { spec: ComparisonSpec, rows: Array<ComparisonRow>, };

// prettier-ignore
export type Report = { schema_version: number, title: string, comparison: Comparison, created_at: string, renderer: string, charts: Array<Chart>, };

// prettier-ignore
export type RunQuery = { search: string, sort: string, descending: boolean, columns: Array<string> | null, limit: number, cursor: string | null, };

// prettier-ignore
export type RunSummary = { id: string, digest: string, origin: ResultOrigin, name: string | null, status: string, base_commit: string, started_at: string, ended_at: string | null, parameters: Array<Datum>, metrics: Array<Datum>, capture_issues: Array<string>, };

// prettier-ignore
export type Column = { key: string, file: string, pointer: string, kind: string, };

// prettier-ignore
export type RunPage = { generation: string, total: number, columns: Array<Column>, display_columns: Array<string>, rows: Array<RunSummary>, next_cursor: string | null, };

// prettier-ignore
export type Chart = { key: string, title: string, state: Availability, reason: string | null, spec: JsonValue | null, points: number, displayed_points: number, reduction: ChartReduction | null, interaction: ChartInteraction | null, images: Array<ChartImage>, };

// prettier-ignore
export type ChartReduction = "sampled" | "counted";

// prettier-ignore
export type ChartImage = { experiment_id: string, digest: string, path: string, hash: string, label: string, };

// prettier-ignore
export type RunSelection = { id: string, digest: string, };

// prettier-ignore
export type ResultsRead = { "kind": "list", "args": RunQuery } | { "kind": "run", "args": RunSelection } | { "kind": "compare", "args": { runs: Array<RunSelection>, spec: ComparisonSpec, } } | { "kind": "reports" } | { "kind": "report", "args": { id: string, } } | { "kind": "evidence", "args": { run: RunSelection, path: string, report: string | null, } };

// prettier-ignore
export type ComparisonView = { comparison: Comparison, charts: Array<Chart>, };

// prettier-ignore
export type ReportView = { id: string, report: Report, charts: Array<Chart>, };

// prettier-ignore
export type EvidenceView = { attachment: Attachment, text: string | null, data_url: string | null, reason: string | null, };

// prettier-ignore
export type ResultsResponse = { "kind": "page", "data": RunPage } | { "kind": "run", "data": SealedResult } | { "kind": "comparison", "data": ComparisonView } | { "kind": "reports", "data": Array<string> } | { "kind": "report", "data": ReportView } | { "kind": "evidence", "data": EvidenceView };

// prettier-ignore
export type ReportDraft = { title: string, runs: Array<RunSelection>, spec: ComparisonSpec, };

// prettier-ignore
export type ArtifactPromotionRequest = { name: string, version: string, stage: string, expected: string | null, };

// prettier-ignore
export type ActionPreview = { base_commit: string, checkpoint: string | null, applied: Array<string>, deleted: Array<string>, };
