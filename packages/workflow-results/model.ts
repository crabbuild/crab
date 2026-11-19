import type {
  RunQuery,
  ComparisonSpec,
  RunSelection,
  Datum,
  SealedResult,
} from "./contracts";
export const initialQuery: RunQuery = {
  search: "",
  sort: "started_at",
  descending: true,
  columns: null,
  limit: 50,
  cursor: null,
};
export const initialSpec: ComparisonSpec = {
  baseline: "",
  columns: null,
  objectives: {},
  datasets: {},
  include_logs: false,
  axes: {},
};
export interface Preferences {
  query: RunQuery;
  selected: RunSelection[];
  spec: ComparisonSpec;
  report: string;
}
export function restore(value: unknown): Preferences {
  const empty = {
    query: initialQuery,
    selected: [],
    spec: initialSpec,
    report: "",
  };
  if (!value || typeof value !== "object") return empty;
  const source = value as Partial<Preferences>;
  if (
    JSON.stringify(value).length > 65536 ||
    !Array.isArray(source.selected) ||
    source.selected.length > 20
  )
    return empty;
  if (
    !source.selected.every(
      (run) =>
        run !== null &&
        typeof run === "object" &&
        typeof run.id === "string" &&
        /^[0-9a-f-]{36}$/.test(run.id) &&
        /^[0-9a-f]{64}$/.test(run.digest),
    )
  )
    return empty;
  if (
    !source.query ||
    typeof source.query.sort !== "string" ||
    source.query.sort.length > 8192 ||
    typeof source.query.descending !== "boolean" ||
    typeof source.query.search !== "string" ||
    source.query.search.length > 1024 ||
    (source.query.columns !== null &&
      (!Array.isArray(source.query.columns) ||
        source.query.columns.length > 32 ||
        !source.query.columns.every(
          (key) => typeof key === "string" && key.length <= 8192,
        )))
  )
    return empty;
  if (
    !source.spec ||
    typeof source.spec.baseline !== "string" ||
    (source.spec.columns !== null &&
      (!Array.isArray(source.spec.columns) ||
        source.spec.columns.length > 32 ||
        !source.spec.columns.every(
          (key) => typeof key === "string" && key.length <= 8192,
        ))) ||
    typeof source.spec.include_logs !== "boolean" ||
    !source.spec.objectives ||
    typeof source.spec.objectives !== "object" ||
    Array.isArray(source.spec.objectives) ||
    !source.spec.datasets ||
    typeof source.spec.datasets !== "object" ||
    Array.isArray(source.spec.datasets)
  )
    return empty;
  if (
    Object.keys(source.spec.objectives).length > 32 ||
    Object.entries(source.spec.objectives).some(
      ([key, value]) =>
        key.length > 8192 || !["minimize", "maximize"].includes(value),
    )
  )
    return empty;
  if (
    Object.keys(source.spec.datasets).length > 20 ||
    Object.entries(source.spec.datasets).some(
      ([id, value]) =>
        !/^[0-9a-f-]{36}$/.test(id) ||
        !value ||
        typeof value !== "object" ||
        !/^[0-9a-f]{64}$/.test(value.hash) ||
        typeof value.split !== "string" ||
        value.split.length > 256 ||
        typeof value.evaluator !== "string" ||
        value.evaluator.length > 256,
    )
  )
    return empty;
  if (
    !source.spec.axes ||
    typeof source.spec.axes !== "object" ||
    Array.isArray(source.spec.axes) ||
    Object.keys(source.spec.axes).length > 32 ||
    Object.entries(source.spec.axes).some(
      ([key, range]) =>
        !/^[0-9a-f]{64}$/.test(key) ||
        !range ||
        typeof range !== "object" ||
        !Number.isFinite(range.lower) ||
        !Number.isFinite(range.upper) ||
        range.lower >= range.upper ||
        !Number.isFinite(range.upper - range.lower),
    )
  )
    return empty;
  return {
    query: {
      search: source.query.search,
      sort: source.query.sort,
      descending: source.query.descending,
      columns: source.query.columns,
      cursor: null,
      limit: 50,
    },
    selected: source.selected.map(({ id, digest }) => ({ id, digest })),
    spec: {
      baseline: source.spec.baseline,
      columns: source.spec.columns,
      objectives: source.spec.objectives,
      datasets: Object.fromEntries(
        Object.entries(source.spec.datasets).map(([id, value]) => [
          id,
          { hash: value.hash, split: value.split, evaluator: value.evaluator },
        ]),
      ),
      include_logs: source.spec.include_logs,
      axes: Object.fromEntries(
        Object.entries(source.spec.axes).map(([key, range]) => [
          key,
          { lower: range.lower, upper: range.upper },
        ]),
      ),
    },
    report:
      typeof source.report === "string" && /^[0-9a-f]{64}$/.test(source.report)
        ? source.report
        : "",
  };
}
export function datum(value: Datum | undefined): string {
  if (!value) return "Missing";
  if (value.state !== "present") return value.state.replaceAll("_", " ");
  return JSON.stringify(value.value);
}
export function metricKey(key: string): string {
  return key.startsWith("metric:") ? key.slice(7) : key;
}
export function label(key: string): string {
  try {
    const [file, pointer] = JSON.parse(
      metricKey(key.replace(/^param:/, "")),
    ) as [string, string];
    return `${file}${pointer}`;
  } catch {
    return key;
  }
}

// UUIDv7 prefixes encode time; neighboring runs need the random suffix to differ.
export function shortId(id: string): string {
  return id.slice(-8);
}

const importedActionReason =
  "Imported experiment metadata has no verified execution or workspace capture. Run or save a new experiment to enable actions.";

export function actionReasons(
  run: SealedResult,
  immutable: boolean,
  canExecute: boolean,
): string[] {
  const originReasons =
    run.snapshot.origin === "experiment_metadata" ? [importedActionReason] : [];
  if (immutable)
    return [
      ...originReasons,
      "Execution actions are unavailable in an immutable report. Open this repository locally in Crab Desktop and choose a run in Working comparison, or use the CLI.",
    ];
  if (!canExecute)
    return [
      ...originReasons,
      "Execution actions require a local worktree. Open this repository in Crab Desktop or use the CLI.",
    ];
  return [
    ...new Set(
      ["apply", "promote", "branch", "resume", "reproduce"]
        .map((name) => actionReason(run, name))
        .filter((reason): reason is string => Boolean(reason)),
    ),
  ];
}

export function actionReason(
  run: SealedResult,
  name: string,
): string | undefined {
  if (run.snapshot.origin === "experiment_metadata" && name !== "publish")
    return importedActionReason;
  if (
    (name === "apply" || name === "branch") &&
    !["success", "saved"].includes(run.snapshot.status)
  )
    return "Apply and branch creation require a successful run or a saved workspace.";
  if (name === "promote" && run.snapshot.status !== "success")
    return "Artifact promotion requires a successful run with captured output evidence.";
  if (
    name === "resume" &&
    !run.snapshot.attachments.some(
      (attachment) =>
        attachment.kind === "checkpoint" && attachment.state === "present",
    )
  )
    return "Resume is unavailable: this run has no retained checkpoint.";
  if (name === "reproduce") {
    if (!run.snapshot.reproduction)
      return "Reproduce is unavailable: effective execution inputs were not captured.";
    if (run.snapshot.reproduction.unavailable_reasons.length)
      return `Reproduce is unavailable: ${run.snapshot.reproduction.unavailable_reasons.join("; ")}`;
  }
  return undefined;
}
