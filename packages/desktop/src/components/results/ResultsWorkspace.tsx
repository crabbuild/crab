import {
  restore,
  shortId,
  datum,
  metricKey,
  label,
  type Preferences,
} from "../../../../workflow-results/model";
import { ComparisonTable } from "./ComparisonTable";
import { RunDetails, ActionPreviewDialog } from "./RunDetails";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { BarChart3, RefreshCw, Save } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { requestPrompt } from "../../lib/application-dialogs";
import { useSession } from "../../lib/session-context";
import { repositoryKey, type OpenRepo } from "../../state";
import { isCancelledError } from "../../ipc";
import {
  readResults,
  resultsAction,
  saveResultsReport,
} from "../../lib/results-api";
import type {
  ArtifactPromotionRequest,
  ActionPreview,
  AxisRange,
  ChartImage,
  ComparisonSpec,
  ComparisonView,
  EvidenceView,
  ReportView,
  RunPage,
  RunQuery,
  RunSelection,
  SealedResult,
} from "../../lib/results-contracts";
import { ResultChart } from "./Chart";

type ResultFailure = {
  message: string;
  reason?: string;
  recovery?: string;
  diagnostics?: unknown;
};

function resultFailure(error: unknown, preview?: "apply" | "resume"): ResultFailure {
  const data =
    error && typeof error === "object" && "data" in error ? error.data : undefined;
  const command =
    data && typeof data === "object" && "error" in data && data.error
      && typeof data.error === "object" ? data.error : null;
  let message = error instanceof Error ? error.message : String(error);
  if (command && "message" in command && typeof command.message === "string") {
    message = command.message;
  }
  if (!preview) return { message, diagnostics: data };
  const action = preview === "apply" ? "Apply" : "Resume";
  const corruption = command && "code" in command && command.code === "CRAB-E0020";
  return {
    message: `${action} preview refused. Workspace files were not changed.`,
    reason: message,
    recovery: corruption
      ? `Restore or retrieve the original experiment snapshot, then choose ${action} again.`
      : `Resolve the problem shown here, then choose ${action} again.`,
    diagnostics: data,
  };
}

export function ResultsWorkspace({ repo }: { repo: OpenRepo }) {
  const session = useSession();
  const identity = repositoryKey(repo);
  // Display/capability updates do not replace the native read scope. Keeping
  // it stable avoids remounting focused comparison controls on metadata refresh.
  const scope = useMemo(
    () => ({
      repo_id: repo.repo_id,
      native_session_id: repo.native_session_id,
    }),
    [repo.repo_id, repo.native_session_id],
  );
  const [preferences, setPreferences] = useState<Preferences>(() =>
    restore(session.activate(identity)?.viewState.workflowResultsV1),
  );
  const [cursor, setCursor] = useState<{
    key: string;
    field: string;
    value: unknown;
  } | null>(null);
  const [inspection, setInspection] = useState<{
    report: string;
    axes: Record<string, AxisRange | null>;
  }>({ report: "", axes: {} });
  const [page, setPage] = useState<RunPage | null>(null);
  const [comparing, setComparing] = useState(false);
  const [comparison, setComparison] = useState<ComparisonView | null>(null);
  const [report, setReport] = useState<ReportView | null>(null);
  const [reports, setReports] = useState<string[]>([]);
  const [run, setRun] = useState<SealedResult | null>(null);
  const loadImage = useCallback(
    (image: ChartImage) =>
      readResults(scope, {
        kind: "evidence",
        args: {
          run: { id: image.experiment_id, digest: image.digest },
          path: image.path,
          report: report?.id ?? null,
        },
      }),
    [scope, report?.id],
  );
  const evidenceRequest = useRef<ReturnType<typeof readResults> | null>(null);
  const [evidence, setEvidence] = useState<EvidenceView | null>(null);
  const [error, setError] = useState<ResultFailure | null>(null);
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [preview, setPreview] = useState<{
    name: string;
    details: ActionPreview;
  } | null>(null);
  const [loading, setLoading] = useState(false);
  const [version, refresh] = useState(0);
  const { query, selected, spec } = preferences;
  const requestSpec = useMemo<ComparisonSpec>(
    () => ({
      baseline: spec.baseline,
      columns: spec.columns,
      objectives: spec.objectives,
      datasets: spec.datasets,
      include_logs: spec.include_logs,
      axes: {},
    }),
    [
      spec.baseline,
      spec.columns,
      spec.objectives,
      spec.datasets,
      spec.include_logs,
    ],
  );
  const canAct = Boolean(repo.capabilities?.canExec);
  useEffect(() => {
    const viewState = session.activate(identity)?.viewState ?? {};
    session.update(identity, {
      viewState: { ...viewState, workflowResultsV1: preferences },
    });
    session.flush();
  }, [identity, preferences, session]);
  useEffect(() => {
    let active = true;
    setLoading(true);
    setError(null);
    const request = readResults(scope, { kind: "list", args: query });
    void request.promise
      .then((result) => {
        if (active && result.kind === "page") setPage(result.data);
      })
      .catch((error) => {
        if (active && !isCancelledError(error)) setError(resultFailure(error));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
      request.cancel();
    };
  }, [scope, query, version]);
  useEffect(() => {
    let active = true;
    const request = readResults(scope, { kind: "reports" });
    void request.promise
      .then((result) => {
        if (active && result.kind === "reports") setReports(result.data);
      })
      .catch((error) => {
        if (active && !isCancelledError(error)) setError(resultFailure(error));
      });
    return () => {
      active = false;
      request.cancel();
    };
  }, [scope, version]);
  useEffect(() => {
    let active = true;
    setComparison(null);
    setReport(null);
    setRun((current) =>
      current &&
      !preferences.report &&
      selected.some(
        (selection) =>
          selection.digest === current.digest &&
          selection.id === current.snapshot.experiment_id,
      )
        ? current
        : null,
    );
    setEvidence(null);
    setComparing(Boolean(selected.length || preferences.report));
    if (!selected.length && !preferences.report) return;
    const request = readResults(
      scope,
      preferences.report
        ? { kind: "report", args: { id: preferences.report } }
        : { kind: "compare", args: { runs: selected, spec: requestSpec } },
    );
    void request.promise
      .then((result) => {
        if (!active) return;
        if (result.kind === "comparison") {
          setComparison(result.data);
          const keys = new Set(
            result.data.charts.flatMap((chart) =>
              chart.interaction ? [chart.interaction.key] : [],
            ),
          );
          setPreferences((p) => {
            const axes = Object.fromEntries(
              Object.entries(p.spec.axes).filter(([key]) => keys.has(key)),
            );
            return Object.keys(axes).length === Object.keys(p.spec.axes).length
              ? p
              : { ...p, spec: { ...p.spec, axes } };
          });
          const rows = result.data.comparison.rows;
          setRun(
            (current) =>
              rows.find((row) => row.run.digest === current?.digest)?.run ??
              rows[0]?.run ??
              null,
          );
        }
        if (result.kind === "report") {
          setReport(result.data);
          setRun(result.data.report.comparison.rows[0]?.run ?? null);
        }
      })
      .catch((error) => {
        if (active && !isCancelledError(error)) setError(resultFailure(error));
      })
      .finally(() => {
        if (active) setComparing(false);
      });
    return () => {
      active = false;
      request.cancel();
    };
  }, [scope, selected, requestSpec, preferences.report, version]);
  const updateQuery = (change: Partial<RunQuery>) =>
    setPreferences((p) => ({
      ...p,
      query: { ...p.query, ...change, cursor: null },
    }));
  const updateSpec = (change: Partial<ComparisonSpec>) =>
    setPreferences((p) => ({
      ...p,
      spec: { ...p.spec, ...change },
      report: "",
    }));
  const select = (selection: RunSelection) =>
    setPreferences((p) => {
      const exists = p.selected.some((run) => run.id === selection.id);
      const next = exists
        ? p.selected.filter((run) => run.id !== selection.id)
        : [...p.selected, selection].slice(0, 20);
      return {
        ...p,
        selected: next,
        report: "",
        spec: {
          ...p.spec,
          datasets: Object.fromEntries(
            Object.entries(p.spec.datasets).filter(([id]) =>
              next.some((run) => run.id === id),
            ),
          ),
          baseline: next.some((run) => run.id === p.spec.baseline)
            ? p.spec.baseline
            : (next[0]?.id ?? ""),
        },
      };
    });
  const changeAxis = (key: string, range: AxisRange | null, reset = false) => {
    setCursor(null);
    if (report) {
      setInspection((previous) => {
        const axes = {
          ...(previous.report === report.id ? previous.axes : {}),
        };
        if (reset) delete axes[key];
        else axes[key] = range;
        return { report: report.id, axes };
      });
      return;
    }
    if (
      range &&
      !Object.hasOwn(spec.axes, key) &&
      Object.keys(spec.axes).length >= 32
    ) {
      setError(resultFailure("At most 32 linked axis ranges can be saved."));
      return;
    }
    setPreferences((p) => {
      const axes = { ...p.spec.axes };
      if (range && !reset) axes[key] = range;
      else delete axes[key];
      return { ...p, spec: { ...p.spec, axes } };
    });
  };
  const rangeFor = (key: string) =>
    report
      ? inspection.report === report.id && Object.hasOwn(inspection.axes, key)
        ? inspection.axes[key]
        : (report.report.comparison.spec.axes[key] ?? null)
      : (spec.axes[key] ?? null);
  const updatedSnapshots = selected.filter((selection) =>
    page?.rows.some(
      (row) => row.id === selection.id && row.digest !== selection.digest,
    ),
  );
  const shown = report?.report.comparison ?? comparison?.comparison;
  const charts = report?.charts ?? comparison?.charts ?? [];
  const columns = useMemo(
    () =>
      page?.columns.filter((column) =>
        page.display_columns.includes(column.key),
      ) ?? [],
    [page],
  );
  const action = useCallback(
    async (name: string, confirmed = false) => {
      if (!run || busy) return;
      setNotice("");
      if (!confirmed && (name === "apply" || name === "resume")) {
        setBusy(true);
        setError(null);
        try {
          const details = await resultsAction<ActionPreview>(
            scope,
            `preview_${name}`,
            {
              id: run.snapshot.experiment_id,
              digest: run.digest,
            },
          );
          setPreview({ name, details });
        } catch (error) {
          setError(resultFailure(error, name));
        } finally {
          setBusy(false);
        }
        return;
      }
      setPreview(null);
      const branch =
        name === "branch"
          ? await requestPrompt(
              "Create a Git branch from the experiment snapshot.",
              `exp-${shortId(run.snapshot.experiment_id)}`,
            )
          : undefined;
      if (name === "branch" && !branch) return;
      let promotion: ArtifactPromotionRequest | undefined;
      if (name === "promote") {
        const artifactName = await requestPrompt(
          "Artifact name declared in this repository",
        );
        if (!artifactName) return;
        const version = await requestPrompt(
          "Immutable artifact version (b3:…). Its bytes must match this run's recorded output.",
        );
        if (!version) return;
        const stage = await requestPrompt(
          "Promote artifact to stage",
          "staging",
        );
        if (!stage) return;
        const expected = await requestPrompt(
          "Expected current version for compare-and-swap (leave empty for an unconditional promotion)",
          "",
        );
        if (expected === null) return;
        promotion = {
          name: artifactName,
          version,
          stage,
          expected: expected || null,
        };
      }
      setBusy(true);
      setError(null);
      try {
        await resultsAction(
          scope,
          name,
          { id: run.snapshot.experiment_id, digest: run.digest },
          undefined,
          branch ?? undefined,
          promotion,
        );
        setNotice(`${name} completed`);
        refresh((v) => v + 1);
      } catch (error) {
        setError(resultFailure(error));
      } finally {
        setBusy(false);
      }
    },
    [busy, scope, run],
  );
  const save = async () => {
    const title = await requestPrompt(
      "Save immutable report: title",
      "Experiment comparison",
    );
    if (!title) return;
    setBusy(true);
    setError(null);
    try {
      const saved = await saveResultsReport(scope, {
        title,
        runs: selected,
        spec,
      });
      setPreferences((p) => ({ ...p, report: saved.id }));
      setNotice(`Saved report ${saved.id}`);
      refresh((v) => v + 1);
    } catch (error) {
      setError(resultFailure(error));
    } finally {
      setBusy(false);
    }
  };
  // Two reports can share a run while retaining different log closures.
  useEffect(() => {
    setEvidence(null);
    return () => {
      evidenceRequest.current?.cancel();
      evidenceRequest.current = null;
    };
  }, [run?.digest, report?.id]);
  const openEvidence = async (path: string) => {
    if (!run) return;
    evidenceRequest.current?.cancel();
    const request = readResults(scope, {
      kind: "evidence",
      args: {
        run: { id: run.snapshot.experiment_id, digest: run.digest },
        path,
        report: report?.id ?? null,
      },
    });
    evidenceRequest.current = request;
    setError(null);
    try {
      const result = await request.promise;
      if (evidenceRequest.current === request && result.kind === "evidence")
        setEvidence(result.data);
    } catch (error) {
      if (evidenceRequest.current === request && !isCancelledError(error))
        setError(resultFailure(error));
    } finally {
      if (evidenceRequest.current === request) evidenceRequest.current = null;
    }
  };
  return (
    <div
      className="flex h-full min-h-0 flex-col bg-background text-foreground"
      role="region"
      aria-label="Results workspace"
    >
      <div className="flex min-h-11 flex-wrap items-center gap-2 border-b border-border px-4 py-2">
        <BarChart3 size={16} />
        <h2 className="mr-2 text-sm font-medium">Results</h2>
        <Input
          className="h-8 min-w-40 flex-1"
          aria-label="Search experiments"
          placeholder="Search runs, parameters, metrics…"
          value={query.search}
          onChange={(e) => updateQuery({ search: e.target.value })}
        />
        <Button
          variant="ghost"
          size="sm"
          aria-label="Refresh Results"
          onClick={() => {
            updateQuery({ cursor: null });
            refresh((v) => v + 1);
          }}
        >
          <RefreshCw size={16} />
        </Button>
        <details className="relative">
          <summary className="cursor-pointer text-xs">Columns</summary>
          <div className="fixed left-4 right-4 z-20 mt-2 max-h-72 overflow-auto rounded-md border border-border bg-popover p-3 shadow-md sm:absolute sm:left-auto sm:right-0 sm:w-80">
            {page?.columns.map((column) => (
              <label key={column.key} className="flex gap-2 py-1 text-xs">
                <input
                  type="checkbox"
                  checked={(query.columns ?? page.display_columns).includes(
                    column.key,
                  )}
                  onChange={(e) => {
                    const checked = e.target.checked;
                    setPreferences((p) => {
                      const current = p.query.columns ?? page.display_columns;
                      const columns = checked
                        ? [...current, column.key].slice(0, 32)
                        : current.filter((key) => key !== column.key);
                      return {
                        ...p,
                        query: { ...p.query, columns, cursor: null },
                        spec: {
                          ...p.spec,
                          columns: columns
                            .filter((key) => key.startsWith("metric:"))
                            .map(metricKey),
                        },
                        report: "",
                      };
                    });
                  }}
                />
                {column.kind} · {column.file}
                {column.pointer}
              </label>
            ))}
          </div>
        </details>
        <Button
          variant="outline"
          size="sm"
          disabled={busy}
          onClick={async () => {
            const id = await requestPrompt(
              "Open immutable report: paste its 64-character ID",
            );
            if (!id) return;
            if (!/^[0-9a-f]{64}$/.test(id)) {
              setError(resultFailure("Report ID must be a 64-character BLAKE3 digest"));
              return;
            }
            setBusy(true);
            setError(null);
            try {
              if (canAct)
                await resultsAction(scope, "retrieve_report", undefined, id);
              setPreferences((p) => ({ ...p, report: id }));
              refresh((v) => v + 1);
            } catch (error) {
              setError(resultFailure(error));
            } finally {
              setBusy(false);
            }
          }}
        >
          Retrieve report
        </Button>
        <select
          aria-label="Open immutable report"
          className="h-8 max-w-48 rounded-md border border-input bg-background text-xs"
          value={preferences.report}
          onChange={(e) =>
            setPreferences((p) => ({ ...p, report: e.target.value }))
          }
        >
          <option value="">Working comparison</option>
          {reports.map((id) => (
            <option key={id} value={id}>
              Report {id.slice(0, 12)}
            </option>
          ))}
        </select>
      </div>
      {error && (
        <div
          role="alert"
          className="flex items-start justify-between gap-2 border-b border-border px-4 py-2 text-sm text-destructive"
        >
          <div className="min-w-0 flex-1 break-words">
            <p>{error.message}</p>
            {error.reason && <p className="mt-1">{error.reason}</p>}
            {error.recovery && <p className="mt-1">{error.recovery}</p>}
            {error.diagnostics !== undefined && (
              <details className="mt-2 text-xs text-muted-foreground">
                <summary className="cursor-pointer">Diagnostics</summary>
                <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap break-all text-foreground">
                  {JSON.stringify(error.diagnostics, null, 2)}
                </pre>
              </details>
            )}
          </div>
          <Button
            className="shrink-0"
            size="sm"
            variant="ghost"
            onClick={() => {
              setError(null);
              updateQuery({});
              refresh((v) => v + 1);
            }}
          >
            Refresh view
          </Button>
        </div>
      )}
      {notice && (
        <p
          role="status"
          className="break-all px-4 py-2 text-xs text-muted-foreground"
        >
          {notice}
        </p>
      )}
      {!report && updatedSnapshots.length > 0 && (
        <div className="flex flex-wrap items-center gap-2 border-b border-border px-4 py-2 text-xs text-muted-foreground">
          <span>
            {updatedSnapshots.length} selected runs have newer snapshots. This
            comparison keeps the captured selection.
          </span>
          <Button
            size="sm"
            variant="outline"
            onClick={() =>
              setPreferences((current) => ({
                ...current,
                report: "",
                selected: current.selected.map((selection) => {
                  const latest = page?.rows.find(
                    (row) => row.id === selection.id,
                  );
                  return latest
                    ? { id: latest.id, digest: latest.digest }
                    : selection;
                }),
              }))
            }
          >
            Use latest snapshots
          </Button>
        </div>
      )}
      <div className="min-h-0 flex-1 overflow-auto">
        {report && !page?.rows.length ? (
          <p className="border-b border-border px-4 py-2 text-xs text-muted-foreground">
            Showing a retrieved report. Choose Working comparison above to
            browse captured runs.
          </p>
        ) : (
          <>
            <div className="overflow-x-auto" aria-busy={loading}>
              <table className="w-full border-collapse text-left text-xs tabular-nums">
                <thead className="bg-muted">
                  <tr>
                    <th className="p-3">
                      <span className="sr-only">Compare</span>
                    </th>
                    {[
                      "name",
                      "status",
                      "started_at",
                      ...columns.map((c) => c.key),
                    ].map((key) => (
                      <th
                        key={key}
                        scope="col"
                        className="whitespace-nowrap px-3 py-2"
                        aria-sort={
                          query.sort === key
                            ? query.descending
                              ? "descending"
                              : "ascending"
                            : "none"
                        }
                      >
                        <Button
                          variant="ghost"
                          size="sm"
                          onClick={() =>
                            updateQuery({
                              sort: key,
                              descending:
                                query.sort === key ? !query.descending : true,
                            })
                          }
                        >
                          {key === "started_at"
                            ? "Started"
                            : key === "name"
                              ? "Experiment"
                              : key === "status"
                                ? "Status"
                                : label(key)}
                        </Button>
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {page?.rows.map((row) => (
                    <tr
                      key={row.id}
                      className="border-b border-border hover:bg-muted/50"
                    >
                      <td className="p-3">
                        <input
                          aria-label={`Compare ${row.name ?? row.id}`}
                          type="checkbox"
                          checked={selected.some((run) => run.id === row.id)}
                          disabled={
                            !selected.some((run) => run.id === row.id) &&
                            selected.length >= 20
                          }
                          onChange={() =>
                            select({ id: row.id, digest: row.digest })
                          }
                        />
                      </td>
                      <td className="px-3 py-2">
                        <span className="font-medium">
                          {row.name ?? shortId(row.id)}
                        </span>
                        {row.name && (
                          <span className="ml-2 text-muted-foreground">
                            {shortId(row.id)}
                          </span>
                        )}
                      </td>
                      <td className="px-3">
                        {row.status}{row.origin === "experiment_metadata" && " (imported)"}
                      </td>
                      <td className="whitespace-nowrap px-3">
                        {row.started_at}
                      </td>
                      {columns.map((column) => (
                        <td
                          key={column.key}
                          className="max-w-64 truncate px-3 font-mono"
                          title={datum(
                            [...row.metrics, ...row.parameters].find(
                              (v) =>
                                `${column.kind}:${JSON.stringify([v.file, v.pointer])}` ===
                                column.key,
                            ),
                          )}
                        >
                          {datum(
                            (column.kind === "metric"
                              ? row.metrics
                              : row.parameters
                            ).find(
                              (v) =>
                                JSON.stringify([v.file, v.pointer]) ===
                                metricKey(column.key.replace(/^param:/, "")),
                            ),
                          )}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
              {!page?.rows.length && (
                <div className="px-6 py-10 text-sm text-muted-foreground">
                  {loading
                    ? "Loading captured results…"
                    : error
                      ? "Results are unavailable. Refresh after resolving the error above."
                      : report
                        ? "No local run catalog. The retrieved immutable report is available below."
                        : query.search
                          ? "No results match this search."
                          : "No captured results yet. Queue and start experiments, or retrieve published results."}
                </div>
              )}
            </div>
            <div className="flex items-center justify-between border-b border-border px-4 py-2 text-xs text-muted-foreground">
              <span>
                {page?.total ?? 0} results · {selected.length}/20 selected
              </span>
              <div className="flex gap-2">
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={!query.cursor}
                  onClick={() => updateQuery({})}
                >
                  First page
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  disabled={!page?.next_cursor || loading}
                  onClick={() =>
                    setPreferences((p) => ({
                      ...p,
                      query: { ...p.query, cursor: page?.next_cursor ?? null },
                    }))
                  }
                >
                  Next page
                </Button>
              </div>
            </div>
          </>
        )}
        {comparing && (
          <p role="status" className="px-4 py-2 text-xs text-muted-foreground">
            Loading comparison…
          </p>
        )}
        {(selected.length > 0 || report) && (
          <div className="px-4 py-4">
            <div className="flex flex-wrap items-center gap-3">
              <h3 className="text-sm font-medium">
                {report ? report.report.title : "Comparison"}
              </h3>
              {report && (
                <span className="text-xs text-muted-foreground">
                  {report.report.comparison.rows.length} runs · Immutable report
                </span>
              )}
              {!report && (
                <>
                  <label className="text-xs">
                    Baseline{" "}
                    <select
                      aria-label="Baseline run"
                      className="ml-2 rounded border border-input bg-background p-1"
                      value={spec.baseline}
                      onChange={(e) => updateSpec({ baseline: e.target.value })}
                    >
                      {selected.map((run) => (
                        <option key={run.id} value={run.id}>
                          {shortId(run.id)}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className="text-xs">
                    <input
                      type="checkbox"
                      checked={spec.include_logs}
                      onChange={(e) =>
                        updateSpec({ include_logs: e.target.checked })
                      }
                    />{" "}
                    Include logs in report
                  </label>
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={!canAct || !comparison || busy}
                    onClick={save}
                  >
                    <Save size={14} /> Save report
                  </Button>
                </>
              )}
              {report && (
                <Button
                  size="sm"
                  variant="outline"
                  disabled={!canAct || busy}
                  onClick={async () => {
                    setBusy(true);
                    try {
                      await resultsAction(
                        scope,
                        "publish_report",
                        undefined,
                        report.id,
                      );
                      setNotice(
                        "Report published. Share its immutable ID or browser report URL.",
                      );
                    } catch (e) {
                      setError(resultFailure(e));
                    } finally {
                      setBusy(false);
                    }
                  }}
                >
                  Publish report
                </Button>
              )}
            </div>
            {report && (
              <p className="mt-2 break-all font-mono text-xs text-muted-foreground">
                {report.id}
              </p>
            )}
            {shown && (
              <ComparisonTable
                comparison={shown}
                spec={spec}
                selectedDigest={run?.digest}
                immutable={Boolean(report)}
                onChange={updateSpec}
                onSelect={(selected) => {
                  setRun(selected);
                  setEvidence(null);
                }}
              />
            )}
            {run && (
              <RunDetails
                key={`${run.digest}:${report?.id ?? "working"}`}
                run={run}
                report={report}
                spec={spec}
                evidence={evidence}
                canAct={canAct}
                busy={busy}
                onAction={action}
                onEvidence={openEvidence}
                onChange={updateSpec}
              />
            )}
            <div className="mt-4 divide-y divide-border">
              {charts.map((chart) => (
                <ResultChart
                  key={chart.key}
                  chart={chart}
                  immutable={Boolean(report)}
                  loadImage={loadImage}
                  range={
                    chart.interaction ? rangeFor(chart.interaction.key) : null
                  }
                  resetRange={
                    chart.interaction && report
                      ? (report.report.comparison.spec.axes[
                          chart.interaction.key
                        ] ?? null)
                      : null
                  }
                  onRange={changeAxis}
                  onReset={(key) => changeAxis(key, null, true)}
                  cursor={cursor}
                  onCursor={setCursor}
                />
              ))}
            </div>
          </div>
        )}
      </div>
      {preview && (
        <ActionPreviewDialog
          name={preview.name}
          preview={preview.details}
          onClose={() => setPreview(null)}
          onConfirm={() => void action(preview.name, true)}
        />
      )}
    </div>
  );
}
