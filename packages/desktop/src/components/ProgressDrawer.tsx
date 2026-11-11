import { useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import {
  Activity,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Clock,
  Filter,
  Loader2,
  Search,
  Trash2,
  X,
  XCircle,
  Ban,
} from "lucide-react";
import { useStore, JobState } from "../state";
import { rpc } from "../ipc";
import { formatBytes } from "../lib/format";

interface Props {
  open: boolean;
  onClose: () => void;
}

type FilterOp = "all" | string;

function formatElapsed(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const secs = Math.floor(ms / 1000);
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  const remSecs = secs % 60;
  return `${mins}m ${remSecs}s`;
}

function formatTime(ts: number): string {
  const d = new Date(ts);
  return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

export function ProgressDrawer({ open, onClose }: Props) {
  const { state, clearLogs } = useStore();
  const scrollRef = useRef<HTMLDivElement>(null);
  const [filterOp, setFilterOp] = useState<FilterOp>("all");
  const [searchQuery, setSearchQuery] = useState("");
  const [showSearch, setShowSearch] = useState(false);
  const [expandedJobs, setExpandedJobs] = useState<Set<string>>(new Set());
  const [autoScroll, setAutoScroll] = useState(true);

  // Derive unique operation types for filter chips
  const opTypes = useMemo(() => {
    const ops = new Set<string>();
    for (const log of state.logs) ops.add(log.op);
    for (const job of Object.values(state.jobs)) ops.add(job.op);
    return Array.from(ops).sort();
  }, [state.logs, state.jobs]);

  // Filter logs
  const filteredLogs = useMemo(() => {
    let logs = state.logs;
    if (filterOp !== "all") {
      logs = logs.filter((l) => l.op === filterOp);
    }
    if (searchQuery.trim()) {
      const q = searchQuery.toLowerCase();
      logs = logs.filter((l) => l.line.toLowerCase().includes(q));
    }
    return logs;
  }, [state.logs, filterOp, searchQuery]);

  // Job entries sorted: running first, then by most recent
  const jobEntries = useMemo(() => {
    const entries = Object.entries(state.jobs);
    if (filterOp !== "all") {
      return entries.filter(([, j]) => j.op === filterOp);
    }
    return entries.sort((a, b) => {
      // Running jobs first
      if (a[1].status === "running" && b[1].status !== "running") return -1;
      if (b[1].status === "running" && a[1].status !== "running") return 1;
      // Then by most recent
      return b[1].updatedAt - a[1].updatedAt;
    });
  }, [state.jobs, filterOp]);

  // Auto-scroll to bottom when new lines arrive
  useEffect(() => {
    if (!open || !scrollRef.current || !autoScroll) return;
    scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
  }, [filteredLogs.length, open, autoScroll]);

  // Detect manual scroll to disable auto-scroll
  const handleScroll = () => {
    if (!scrollRef.current) return;
    const { scrollTop, scrollHeight, clientHeight } = scrollRef.current;
    const atBottom = scrollHeight - scrollTop - clientHeight < 40;
    setAutoScroll(atBottom);
  };

  const toggleJobExpanded = (jobId: string) => {
    setExpandedJobs((prev) => {
      const next = new Set(prev);
      if (next.has(jobId)) next.delete(jobId);
      else next.add(jobId);
      return next;
    });
  };

  const runningCount = Object.values(state.jobs).filter((j) => j.status === "running").length;
  const doneCount = Object.values(state.jobs).filter((j) => j.status === "done").length;
  const failedCount = Object.values(state.jobs).filter((j) => j.status === "failed").length;
  const cancelledCount = Object.values(state.jobs).filter((j) => j.status === "cancelled").length;

  return (
    <div
      className={clsx(
        "fixed inset-x-0 bottom-0 z-40 flex transition-transform duration-200",
        open ? "translate-y-0" : "translate-y-full",
      )}
    >
      <div className="mx-auto w-full border-t border-gx-border bg-gx-surface shadow-2xl">
        {/* Header */}
        <div className="flex h-9 items-center justify-between border-b border-gx-border px-3 text-xs">
          <div className="flex items-center gap-3">
            <span className="flex items-center gap-1.5 font-semibold">
              <Activity size={12} />
              Activity
            </span>
            <div className="flex items-center gap-2 text-[10px]">
              {runningCount > 0 && (
                <span className="flex items-center gap-1 rounded bg-gx-warn/15 px-1.5 py-0.5 text-gx-warn">
                  <Loader2 size={9} className="animate-spin" />
                  {runningCount} running
                </span>
              )}
              {doneCount > 0 && (
                <span className="flex items-center gap-1 rounded bg-gx-success/15 px-1.5 py-0.5 text-gx-success">
                  <CheckCircle2 size={9} />
                  {doneCount} done
                </span>
              )}
              {failedCount > 0 && (
                <span className="flex items-center gap-1 rounded bg-gx-error/15 px-1.5 py-0.5 text-gx-error">
                  <XCircle size={9} />
                  {failedCount} failed
                </span>
              )}
              {cancelledCount > 0 && <span className="text-gx-muted">{cancelledCount} cancelled</span>}
              <span className="text-gx-muted">
                {filteredLogs.length} line{filteredLogs.length === 1 ? "" : "s"}
              </span>
            </div>
          </div>
          <div className="flex items-center gap-1.5">
            {/* Search toggle */}
            <button
              type="button"
              className={clsx("btn-icon", showSearch && "text-gx-accent")}
              onClick={() => setShowSearch((v) => !v)}
              title="Search logs"
            >
              <Search size={12} />
            </button>
            {/* Clear */}
            <button type="button" className="btn-icon" onClick={clearLogs} title="Clear all logs">
              <Trash2 size={12} />
            </button>
            {/* Close */}
            <button type="button" className="btn-icon" onClick={onClose} aria-label="Close activity drawer">
              <X size={12} />
            </button>
          </div>
        </div>

        {/* Search bar (conditional) */}
        {showSearch && (
          <div className="flex items-center gap-2 border-b border-gx-border bg-gx-bg-sub px-3 py-1.5">
            <Search size={11} className="text-gx-muted" />
            <input
              type="text"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Filter log output…"
              className="flex-1 bg-transparent text-[11px] text-gx-text placeholder:text-gx-muted/60 focus:outline-none"
              autoFocus
            />
            {searchQuery && (
              <button type="button" onClick={() => setSearchQuery("")} className="text-gx-muted hover:text-gx-text">
                <X size={11} />
              </button>
            )}
          </div>
        )}

        {/* Filter chips */}
        {opTypes.length > 1 && (
          <div className="flex items-center gap-1.5 border-b border-gx-border bg-gx-bg-sub px-3 py-1">
            <Filter size={10} className="text-gx-muted" />
            <button
              type="button"
              onClick={() => setFilterOp("all")}
              className={clsx(
                "rounded px-1.5 py-0.5 text-[10px] transition",
                filterOp === "all"
                  ? "bg-gx-accent/20 text-gx-accent"
                  : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
              )}
            >
              All
            </button>
            {opTypes.map((op) => (
              <button
                key={op}
                type="button"
                onClick={() => setFilterOp(op === filterOp ? "all" : op)}
                className={clsx(
                  "rounded px-1.5 py-0.5 text-[10px] font-mono transition",
                  filterOp === op
                    ? "bg-gx-accent/20 text-gx-accent"
                    : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
                )}
              >
                {op}
              </button>
            ))}
          </div>
        )}

        {/* Body: Job cards + log output */}
        <div className="flex h-64">
          {/* Job cards sidebar */}
          {jobEntries.length > 0 && (
            <div className="w-56 shrink-0 overflow-auto border-r border-gx-border bg-gx-bg-sub p-2">
              <div className="mb-1 text-[9px] font-medium uppercase tracking-wider text-gx-muted">
                Jobs
              </div>
              {jobEntries.map(([id, job]) => (
                <JobCard
                  key={id}
                  jobId={id}
                  job={job}
                  expanded={expandedJobs.has(id)}
                  onToggle={() => toggleJobExpanded(id)}
                  onFilter={() => setFilterOp(job.op)}
                />
              ))}
            </div>
          )}

          {/* Log output */}
          <div
            ref={scrollRef}
            className="min-w-0 flex-1 overflow-auto bg-gx-bg-sub p-3 font-mono text-[11px] leading-5"
            onScroll={handleScroll}
          >
            {filteredLogs.length === 0 && (
              <div className="flex h-full items-center justify-center text-gx-muted text-[12px]">
                {state.logs.length === 0
                  ? jobEntries.length > 0
                    ? "No command output. Transfer progress is shown with each job."
                    : "No activity yet. Clone, push, or hydrate a repo to see progress here."
                  : "No logs match the current filter."}
              </div>
            )}
            {filteredLogs.map((line, i) => (
              <div key={i} className="flex gap-2 hover:bg-gx-surface-hover/30 rounded px-1 -mx-1">
                <span className="shrink-0 select-none text-gx-muted/50 tabular-nums w-[60px]">
                  {formatTime(Date.now())}
                </span>
                <span className="shrink-0 rounded bg-gx-accent/10 px-1 text-[10px] text-gx-accent">
                  {line.op}
                </span>
                <span className="shrink-0 text-gx-muted/40 font-mono text-[10px]">
                  {line.job_id.slice(-6)}
                </span>
                <span
                  className={clsx(
                    "min-w-0 whitespace-pre-wrap break-words",
                    line.stream === "stderr" && "text-gx-warn",
                  )}
                >
                  {line.line}
                </span>
              </div>
            ))}
            {/* Auto-scroll indicator */}
            {!autoScroll && filteredLogs.length > 0 && (
              <button
                type="button"
                className="sticky bottom-2 left-1/2 -translate-x-1/2 rounded-full bg-gx-accent/90 px-3 py-1 text-[10px] text-white shadow-lg hover:bg-gx-accent"
                onClick={() => {
                  setAutoScroll(true);
                  scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight, behavior: "smooth" });
                }}
              >
                ↓ New output
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

// --- Job Card ---

interface JobCardProps {
  jobId: string;
  job: JobState;
  expanded: boolean;
  onToggle: () => void;
  onFilter: () => void;
}

function JobCard({ jobId, job, expanded, onToggle, onFilter }: JobCardProps) {
  const elapsed = job.updatedAt - job.startedAt;
  const [cancelRequested, setCancelRequested] = useState(false);
  const [cancelError, setCancelError] = useState<string | null>(null);
  const transfer = job.transfer;
  const dehydrating = job.op === "dehydrate";
  const transferBytes = transfer?.checked_bytes ?? transfer?.written_bytes ?? 0;
  const activity = dehydrating ? "dehydration" : "materialization";
  const completed = dehydrating ? "dehydrated" : "materialized";
  let outcome = dehydrating ? "Dehydration failed" : "Materialization failed";
  if (job.status === "done") outcome = `Verified and ${completed}`;
  if (job.status === "cancelled") outcome = transfer?.files_published ? `Remaining ${activity} cancelled` : "Cancelled before publication";
  if (job.status === "interrupted") outcome = "Outcome unknown. Check the working files.";
  async function cancel() {
    if (!transfer) return;
    setCancelRequested(true);
    setCancelError(null);
    try { await rpc.cancelOperation(transfer.repo_id, jobId); }
    catch (error) {
      setCancelRequested(false);
      setCancelError(error instanceof Error ? error.message : String(error));
    }
  }

  return (
    <div
      className={clsx(
        "mb-1 rounded border transition",
        job.status === "running"
          ? "border-gx-warn/30 bg-gx-warn/5"
          : job.status === "failed"
            ? "border-gx-error/30 bg-gx-error/5"
            : "border-gx-border/50 bg-gx-surface/50",
      )}
    >
      <button
        type="button"
        className="flex w-full items-center gap-1.5 px-2 py-1.5 text-left"
        onClick={onToggle}
      >
        {job.status === "running" ? (
          <Loader2 size={10} className="shrink-0 animate-spin text-gx-warn" />
        ) : job.status === "failed" ? (
          <XCircle size={10} className="shrink-0 text-gx-error" />
        ) : job.status === "cancelled" || job.status === "interrupted" ? (
          <Ban size={10} className="shrink-0 text-gx-muted" />
        ) : (
          <CheckCircle2 size={10} className="shrink-0 text-gx-success" />
        )}
        <span className="min-w-0 flex-1 truncate text-[10px] font-medium text-gx-text">
          {job.op}
        </span>
        {expanded ? <ChevronDown size={9} className="text-gx-muted" /> : <ChevronRight size={9} className="text-gx-muted" />}
      </button>
      {transfer && <div className="space-y-1 px-2 pb-2 text-[10px] text-gx-muted">
        <p className="tabular-nums">{job.status === "running" && transfer.total_bytes === 0 && transfer.files_total ? "Checking selection…" : <>{formatBytes(transferBytes)} / {formatBytes(transfer.total_bytes)} {dehydrating ? "checked" : "written"}</>}</p>
        {transfer.files_published != null && transfer.files_total != null && (
          <p>{transfer.files_published} of {transfer.files_total} files {completed}</p>
        )}
        {job.status === "running" ? <>
          <progress aria-label={dehydrating ? "Dehydration bytes checked" : "Materialization bytes written"} className="h-1 w-full" value={transferBytes} max={Math.max(1, transfer.total_bytes)} />
          <button type="button" className="btn text-[10px]" disabled={cancelRequested} onClick={() => void cancel()}>{cancelRequested ? "Cancellation requested…" : `Cancel ${activity}`}</button>
        </> : <p>{outcome}</p>}
        {cancelError && <p role="alert" className="break-words text-gx-error">{cancelError}</p>}
      </div>}
      {job.status === "interrupted" && <p className="break-words px-2 pb-2 text-[10px] text-gx-warn">{job.interruption}</p>}
      {expanded && (
        <div className="border-t border-gx-border/30 px-2 py-1 text-[9px] text-gx-muted">
          <div className="flex items-center gap-1">
            <Clock size={8} />
            <span>{formatElapsed(elapsed)}</span>
          </div>
          <div className="mt-0.5">
            {job.lines} line{job.lines === 1 ? "" : "s"} · id: {jobId.slice(-8)}
          </div>
          {job.status === "failed" && job.exitCode !== undefined && (
            <div className="mt-0.5 text-gx-error">Exit code: {job.exitCode}</div>
          )}
          <button
            type="button"
            className="mt-1 text-[9px] text-gx-accent hover:underline"
            onClick={(e) => { e.stopPropagation(); onFilter(); }}
          >
            Filter to this op →
          </button>
        </div>
      )}
    </div>
  );
}
