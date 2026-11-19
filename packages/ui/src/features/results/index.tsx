import {
  restore,
  shortId,
  datum,
  metricKey,
  label,
  type Preferences,
} from "../../../../workflow-results/model";
import { ComparisonTable } from "./ComparisonTable";
import { RunDetails } from "./RunDetails";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { BarChart3, RefreshCw, Save } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

import type { ResultsRepository } from "./api";
const isCancelledError = (error: unknown) =>
  error instanceof DOMException && error.name === "AbortError";
import { readResults, saveResultsReport } from "./api";
import type {
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
} from "@/lib/results-contracts";
import { ResultChart } from "./chart";

export default function ResultsWorkspace({
  repo,
  account,
  reportId,
}: {
  repo: ResultsRepository;
  account: string;
  reportId: string | null;
}) {
  const identity = `crab.results.v1.${JSON.stringify([account, repo.id])}`;
  // Account/repository changes remount this workspace. Display metadata can
  // refresh independently without replacing focused comparison controls.
  const scope = useMemo(
    () => ({ owner: repo.owner, name: repo.name }),
    [repo.owner, repo.name],
  );
  const session = useMemo(
    () => ({
      activate: (key: string) => {
        try {
          return {
            viewState: {
              workflowResultsV1: JSON.parse(
                localStorage.getItem(key) ?? "null",
              ) as unknown,
            },
          };
        } catch {
          return null;
        }
      },
      update: (key: string, value: { viewState: Record<string, unknown> }) =>
        localStorage.setItem(
          key,
          JSON.stringify(value.viewState.workflowResultsV1),
        ),
      flush: () => undefined,
    }),
    [],
  );
  const [preferences, setPreferences] = useState<Preferences>(() => ({
    ...restore(session.activate(identity)?.viewState.workflowResultsV1),
    ...(reportId ? { report: reportId } : {}),
  }));
  useEffect(() => {
    if (reportId) setPreferences((p) => ({ ...p, report: reportId }));
  }, [reportId]);
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
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const [version, refresh] = useState(0);
  const { query, selected, spec } = preferences;
  const canAct = false;
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
  const canSaveReport = repo.access === "write" && !repo.archived;
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
    setError("");
    const request = readResults(scope, { kind: "list", args: query });
    void request.promise
      .then((result) => {
        if (active && result.kind === "page") setPage(result.data);
      })
      .catch((error) => {
        if (active && !isCancelledError(error)) setError(String(error));
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
        if (active && !isCancelledError(error)) setError(String(error));
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
        if (active && !isCancelledError(error)) setError(String(error));
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
      setError("At most 32 linked axis ranges can be saved.");
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
  const save = async () => {
    const title = await Promise.resolve(
      window.prompt("Save immutable report: title", "Experiment comparison"),
    );
    if (!title) return;
    setBusy(true);
    setError("");
    try {
      const saved = await saveResultsReport(repo, {
        title,
        runs: selected,
        spec,
      });
      setPreferences((p) => ({ ...p, report: saved.id }));
      setNotice(`Saved report ${saved.id}`);
      refresh((v) => v + 1);
    } catch (error) {
      setError(String(error));
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
    setError("");
    try {
      const result = await request.promise;
      if (evidenceRequest.current === request && result.kind === "evidence")
        setEvidence(result.data);
    } catch (error) {
      if (evidenceRequest.current === request && !isCancelledError(error))
        setError(String(error));
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
          className="flex items-center justify-between gap-2 border-b border-border px-4 py-2 text-sm text-destructive"
        >
          <span className="min-w-0 flex-1 break-words">{error}</span>
          <Button
            className="shrink-0"
            size="sm"
            variant="ghost"
            onClick={() => {
              setError("");
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
                        {row.status}
                        {row.origin === "experiment_metadata" && " (imported)"}
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
                      ? "Results are unavailable. Retry after resolving the error above."
                      : report
                        ? "No local run catalog. The retrieved immutable report is available below."
                        : query.search
                          ? "No results match this search."
                          : "No captured results yet. Publish captured experiments from Crab Desktop or the CLI to see results here."}
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
                    disabled={!canSaveReport || !comparison || busy}
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
                  onClick={() => {
                    const link = new URL(window.location.href);
                    link.searchParams.set("report", report.id);
                    void navigator.clipboard.writeText(link.href).then(
                      () => setNotice("Report link copied"),
                      (error) => setError(String(error)),
                    );
                  }}
                >
                  Copy report link
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
                onAction={() =>
                  setNotice(
                    "Open this repository in Crab Desktop to execute experiment actions.",
                  )
                }
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
    </div>
  );
}
