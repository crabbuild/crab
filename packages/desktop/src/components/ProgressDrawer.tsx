import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
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
import { useStore, repositoryKey, type OpenRepo, type JobState } from "../state";
import { mainProcess, rpc } from "../ipc";
import { selectionTransfers } from "../hooks/useFileTransfers";
import { localPathName } from "../lib/local-path-name";
import { formatBytes } from "../lib/format";
import type { FileTransferSource } from "../../electron/file-transfers";

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
  const { state, dispatch, clearLogs } = useStore();
  const [reloading, setReloading] = useState(false);
  async function reloadActivity() {
    setReloading(true);
    try { dispatch({ type: "file-transfer-state", activity: await rpc.recoverFileTransferHistory() }); }
    catch (error) { dispatch({ type: "file-transfer-error", error: error instanceof Error ? error.message : String(error) }); }
    finally { setReloading(false); }
  }
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
      id="activity-drawer"
      className={clsx(
        "fixed inset-x-0 bottom-0 z-40 flex transition-transform duration-200",
        open ? "translate-y-0" : "translate-y-full",
      )}
    >
      <div className="mx-auto w-full border-t border-border bg-card shadow-2xl">
        {/* Header */}
        <div className="flex h-9 items-center justify-between border-b border-border px-3 text-xs">
          <div className="flex items-center gap-3">
            <span className="flex items-center gap-1.5 font-semibold">
              <Activity size={12} />
              Activity
            </span>
            <div className="flex items-center gap-2 text-[10px]">
              {runningCount > 0 && (
                <span className="flex items-center gap-1 rounded bg-warning/15 px-1.5 py-0.5 text-warning">
                  <Loader2 size={9} className="animate-spin" />
                  {runningCount} running
                </span>
              )}
              {doneCount > 0 && (
                <span className="flex items-center gap-1 rounded bg-success/15 px-1.5 py-0.5 text-success">
                  <CheckCircle2 size={9} />
                  {doneCount} done
                </span>
              )}
              {failedCount > 0 && (
                <span className="flex items-center gap-1 rounded bg-destructive/15 px-1.5 py-0.5 text-destructive">
                  <XCircle size={9} />
                  {failedCount} failed
                </span>
              )}
              {cancelledCount > 0 && <span className="text-muted-foreground">{cancelledCount} cancelled</span>}
              <span className="text-muted-foreground">
                {filteredLogs.length} line{filteredLogs.length === 1 ? "" : "s"}
              </span>
            </div>
          </div>
          <div className="flex items-center gap-1.5">
            {/* Search toggle */}
            <Button size="icon" variant="ghost"
              type="button"
              className={clsx("", showSearch && "text-primary")}
              onClick={() => setShowSearch((v) => !v)}
              title="Search logs"
            >
              <Search size={12} />
            </Button>
            {/* Clear */}
            <Button size="icon" variant="ghost" type="button" onClick={clearLogs} title="Clear all logs">
              <Trash2 size={12} />
            </Button>
            {/* Close */}
            <Button size="icon" variant="ghost" type="button" onClick={onClose} aria-label="Close activity drawer">
              <X size={12} />
            </Button>
          </div>
        </div>

        {state.activityError && <div role="alert" className="flex items-center gap-3 border-b border-destructive/30 bg-destructive/10 px-3 py-2 text-xs text-destructive">
          <span className="min-w-0 flex-1 break-words">{state.activityError}</span>
          <Button variant="outline" type="button" className="text-xs" disabled={reloading} onClick={() => void reloadActivity()}>{reloading ? "Reloading…" : "Reload Activity"}</Button>
        </div>}

        {/* Search bar (conditional) */}
        {showSearch && (
          <div className="flex items-center gap-2 border-b border-border bg-background px-3 py-1.5">
            <Search size={11} className="text-muted-foreground" />
            <Input
              type="text"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Filter log output…"
              className="flex-1 bg-transparent text-[11px] text-foreground placeholder:text-muted-foreground/60 focus:outline-none"
              autoFocus
            />
            {searchQuery && (
              <button type="button" onClick={() => setSearchQuery("")} className="text-muted-foreground hover:text-foreground">
                <X size={11} />
              </button>
            )}
          </div>
        )}

        {/* Filter chips */}
        {opTypes.length > 1 && (
          <div className="flex items-center gap-1.5 border-b border-border bg-background px-3 py-1">
            <Filter size={10} className="text-muted-foreground" />
            <button
              type="button"
              onClick={() => setFilterOp("all")}
              className={clsx(
                "rounded px-1.5 py-0.5 text-[10px] transition",
                filterOp === "all"
                  ? "bg-primary/20 text-primary"
                  : "text-muted-foreground hover:text-foreground hover:bg-accent",
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
                    ? "bg-primary/20 text-primary"
                    : "text-muted-foreground hover:text-foreground hover:bg-accent",
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
            <div className="w-56 shrink-0 overflow-auto border-r border-border bg-background p-2">
              <div className="mb-1 text-[9px] font-medium uppercase tracking-wider text-muted-foreground">
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
            className="min-w-0 flex-1 overflow-auto bg-background p-3 font-mono text-[11px] leading-5"
            onScroll={handleScroll}
          >
            {filteredLogs.length === 0 && (
              <div className="flex h-full items-center justify-center text-muted-foreground text-[12px]">
                {state.logs.length === 0
                  ? jobEntries.length > 0
                    ? "No command output. Transfer progress is shown with each job."
                    : "No activity yet. Clone, push, or hydrate a repo to see progress here."
                  : "No logs match the current filter."}
              </div>
            )}
            {filteredLogs.map((line, i) => (
              <div key={i} className="flex gap-2 hover:bg-accent/30 rounded px-1 -mx-1">
                <span className="shrink-0 select-none text-muted-foreground/50 tabular-nums w-[60px]">
                  {formatTime(Date.now())}
                </span>
                <span className="shrink-0 rounded bg-primary/10 px-1 text-[10px] text-primary">
                  {line.op}
                </span>
                <span className="shrink-0 text-muted-foreground/40 font-mono text-[10px]">
                  {line.job_id.slice(-6)}
                </span>
                <span
                  className={clsx(
                    "min-w-0 whitespace-pre-wrap break-words",
                    line.stream === "stderr" && "text-warning",
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
                className="sticky bottom-2 left-1/2 -translate-x-1/2 rounded-full bg-primary/90 px-3 py-1 text-[10px] text-foreground shadow-lg hover:bg-primary"
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
  const { state, addRepo, runFileTransfer } = useStore();
  const [retrying, setRetrying] = useState(false);
  const [retryMessage, setRetryMessage] = useState<string | null>(null);
  const [pathPage, setPathPage] = useState(0);
  const [sources, setSources] = useState<{ start: number; items: FileTransferSource[] } | null>(null);
  const [sourceError, setSourceError] = useState<string | null>(null);
  const [sourceRefresh, setSourceRefresh] = useState(0);
  useEffect(() => {
    if (!expanded || !job.intent) return;
    let current = true;
    const start = pathPage * 50;
    setSourceError(null);
    void rpc.fileTransferSources(jobId, start, 50).then(items => {
      if (current) setSources({ start, items });
    }, error => {
      if (current) setSourceError(error instanceof Error ? error.message : String(error));
    });
    return () => { current = false; };
  }, [expanded, jobId, pathPage, job.updatedAt, job.intent, sourceRefresh]);
  const sourcePage = sources?.start === pathPage * 50 ? sources.items : null;
  const byPosition = new Map(sourcePage?.map(source => [source.position, source]));
  const transfer = job.transfer;
  async function retry() {
    const intent = job.intent;
    if (!intent) return;
    setRetrying(true); setRetryMessage(null);
    try {
      let repo = state.repos.find(candidate => repositoryKey(candidate) === intent.repository);
      if (!repo || repo.deferred || !repo.native_session_id) {
        const handle = await rpc.reopen(intent.repository, repo);
        const capabilities = await mainProcess.getCapabilities(handle.repo_id);
        repo = { ...handle, capabilities, path: intent.repository, label: repo?.label ?? localPathName(intent.repository), openedAt: repo?.openedAt ?? Date.now() } satisfies OpenRepo;
        addRepo(repo);
      }
      repo = { ...repo, capabilities: await mainProcess.getCapabilities(repo.repo_id) };
      const storage = await rpc.storageConnections(repo.repo_id);
      const changed = storage.selected_id !== intent.storageConnectionId;
      const source = storage.connections.find(connection => connection.id === storage.selected_id)?.name ?? "default storage";
      const context = `Retry in ${repo.label}, using ${source}${changed ? " (changed since the previous attempt)" : ""}. The current source and eligible paths will be captured again. Files already published stay in place.`;
      const transfers = selectionTransfers(repo, runFileTransfer, true, context);
      const run = transfers[intent.verb];
      if (!run) throw new Error("This repository's current native services do not support the selected transfer. Reopen it and retry.");
      const count = intent.verb === "restore" ? await transfers.restore!(intent.paths)
        : await transfers[intent.verb]!({ kind: "custom", paths: intent.paths });
      if (count === 0) setRetryMessage("No eligible files remain in this selection. Check the current working files.");
      else if (count === null) setRetryMessage("Retry dismissed. No new transfer started.");
    } catch (error) { setRetryMessage(error instanceof Error ? error.message : String(error)); }
    finally { setRetrying(false); }
  }
  const dehydrating = job.op === "dehydrate";
  const transferBytes = transfer?.checked_bytes ?? transfer?.written_bytes ?? 0;
  const activity = job.op === "restore" ? "restoration" : dehydrating ? "dehydration" : "materialization";
  const completed = job.op === "restore" ? "restored" : dehydrating ? "dehydrated" : "materialized";
  let outcome = job.op === "restore" ? "Restoration failed" : dehydrating ? "Dehydration failed" : "Materialization failed";
  if (job.status === "done") outcome = `Verified and ${completed}`;
  if (job.status === "cancelled") outcome = transfer?.files_published ? `Remaining ${activity} cancelled` : "Cancelled before publication";
  if (job.status === "interrupted") outcome = "Outcome unknown. Check the working files.";
  async function cancel() {
    const repoId = job.repoId ?? transfer?.repo_id;
    if (!repoId || !job.nativeSessionId) return;
    setCancelRequested(true);
    setCancelError(null);
    try { await rpc.cancelOperation(repoId, jobId, job.nativeSessionId); }
    catch (error) {
      setCancelError(error instanceof Error ? error.message : String(error));
    } finally { setCancelRequested(false); }
  }

  return (
    <div
      className={clsx(
        "mb-1 rounded border transition",
        job.status === "running"
          ? "border-warning/30 bg-warning/5"
          : job.status === "failed"
            ? "border-destructive/30 bg-destructive/5"
            : "border-border/50 bg-card/50",
      )}
    >
      <button
        type="button"
        className="flex w-full items-center gap-1.5 px-2 py-1.5 text-left"
        onClick={onToggle}
      >
        {job.status === "running" ? (
          <Loader2 size={10} className="shrink-0 animate-spin text-warning" />
        ) : job.status === "failed" ? (
          <XCircle size={10} className="shrink-0 text-destructive" />
        ) : job.status === "cancelled" || job.status === "interrupted" ? (
          <Ban size={10} className="shrink-0 text-muted-foreground" />
        ) : (
          <CheckCircle2 size={10} className="shrink-0 text-success" />
        )}
        <span className="min-w-0 flex-1 truncate text-[10px] font-medium text-foreground">
          {job.op}{job.intent ? ` · ${localPathName(job.intent.repository)}` : ""}
        </span>
        {expanded ? <ChevronDown size={9} className="text-muted-foreground" /> : <ChevronRight size={9} className="text-muted-foreground" />}
      </button>
      {transfer && <div className="space-y-1 px-2 pb-2 text-[10px] text-muted-foreground">
        {job.status === "interrupted" && <p>Last recorded progress</p>}
        <p className="tabular-nums">{job.status === "running" && transfer.total_bytes === 0 && transfer.files_total ? "Checking selection…" : <>{formatBytes(transferBytes)}{transfer.total_bytes === null ? "" : ` / ${formatBytes(transfer.total_bytes)}`} {dehydrating ? "checked" : "written"}</>}</p>
        <p className="tabular-nums" title="Consumed storage response bodies, including metadata and repeated reads. Connection setup and external commands are excluded.">{transfer.received_bytes == null ? "Storage received: not recorded" : `${formatBytes(transfer.received_bytes)} received from storage`}</p>
        {transfer.files_published != null && transfer.files_total != null && (
          <p>{transfer.files_published} of {transfer.files_total} files {completed}</p>
        )}
        {job.status === "running" ? <>
          <progress aria-label={`${activity} bytes ${dehydrating ? "checked" : "written"}`} className="h-1 w-full" value={transfer.total_bytes === null ? undefined : transferBytes} max={transfer.total_bytes === null ? undefined : Math.max(1, transfer.total_bytes)} />

        </> : <p>{outcome}</p>}

      </div>}
      {job.status === "running" && job.intent && <div className="px-2 pb-2 text-[10px] text-muted-foreground">
        {!transfer && <p>Waiting for native admission…</p>}
        <Button variant="outline" type="button" className="text-[10px]" disabled={cancelRequested || job.cancelRequested || !job.nativeSessionId} onClick={() => void cancel()}>{cancelRequested || job.cancelRequested ? "Cancellation requested…" : `Cancel ${activity}`}</Button>
      </div>}
      {job.intent && job.status !== "running" && job.status !== "done" && <div className="px-2 pb-2">
        <Button variant="outline" type="button" className="text-[10px]" disabled={retrying || !!state.activityError} onClick={() => void retry()}>{retrying ? "Preparing retry…" : "Retry transfer"}</Button>
      </div>}
      {retryMessage && <p role="status" className="break-words px-2 pb-2 text-[10px] text-muted-foreground">{retryMessage}</p>}
      {!transfer && job.intent && job.status !== "running" && <p className="px-2 pb-2 text-[10px] text-muted-foreground">{outcome}</p>}
      {cancelError && <p role="alert" className="break-words px-2 pb-2 text-[10px] text-destructive">{cancelError}</p>}
      {job.interruption && <p className="break-words px-2 pb-2 text-[10px] text-warning">{job.interruption}</p>}
      {expanded && (
        <div className="border-t border-border/30 px-2 py-1 text-[9px] text-muted-foreground">
          <div className="flex items-center gap-1">
            <Clock size={8} />
            <span>{formatElapsed(elapsed)}</span>
          </div>
          <div className="mt-0.5">
            {job.lines} log line{job.lines === 1 ? "" : "s"} in this session · id: {jobId.slice(-8)}
          </div>
          {job.intent && <div className="mt-2 space-y-1">
            <p className="break-all">{job.intent.repository}</p>
            <p>{job.intent.paths.length} selected paths · {new Date(job.startedAt).toLocaleString()}</p>
            <p>Captured source versions</p>
            {sourceError ? <p role="alert" className="break-words text-gx-error">{sourceError} <button type="button" className="btn" onClick={() => setSourceRefresh(value => value + 1)}>Reload sources</button></p> : !sourcePage && <p role="status">Loading captured sources…</p>}
            <ul className="max-h-48 space-y-2 overflow-auto">{job.intent.paths.slice(pathPage * 50, (pathPage + 1) * 50).map((path, index) => {
              const source = byPosition.get(pathPage * 50 + index);
              const algorithm = source?.kind === "git" ? "Git blob" : source?.kind === "crab" ? "BLAKE3" : "SHA-256";
              return <li className="break-all" key={path}>
                <p className="font-mono text-gx-text">{path}</p>
                {source ? <>
                  <p>{algorithm} · {formatBytes(source.size)}{source.mode ? ` · ${source.mode}` : ""}</p>
                  <code>{source.hash}</code>
                </> : sourcePage && <p>Source not recorded for this path.</p>}
              </li>;
            })}</ul>
            {job.intent.paths.length > 50 && <div className="flex gap-2">
              <Button variant="outline" type="button" className="text-[9px]" disabled={pathPage === 0} onClick={() => setPathPage(page => page - 1)}>Previous paths</Button>
              <Button variant="outline" type="button" className="text-[9px]" disabled={(pathPage + 1) * 50 >= job.intent.paths.length} onClick={() => setPathPage(page => page + 1)}>Next paths</Button>
            </div>}
          </div>}
          {job.status === "failed" && job.exitCode !== undefined && (
            <div className="mt-0.5 text-destructive">Exit code: {job.exitCode}</div>
          )}
          <button
            type="button"
            className="mt-1 text-[9px] text-primary hover:underline"
            onClick={(e) => { e.stopPropagation(); onFilter(); }}
          >
            Filter to this op →
          </button>
        </div>
      )}
    </div>
  );
}
