import { useState } from "react";
import clsx from "clsx";
import {
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Loader2,
  Package,
  XCircle,
  Zap,
} from "lucide-react";
import { OpenRepo } from "../state";
import { useCrabAddFlow } from "../hooks/useAddFlow";
import { formatBytes, formatDuration } from "../lib/format";

interface Props {
  repo: OpenRepo;
  /** Called after a successful add so the parent can refresh staging stats. */
  onComplete?: () => void;
}

/**
 * Structured add flow panel. Shows per-file chunking progress, phase
 * indicators, and a summary card on completion. Replaces the raw-log
 * experience for `crab add`.
 */
export function AddFlowPanel({ repo, onComplete }: Props) {
  const { state, start, cancel, reset } = useCrabAddFlow();
  const [patterns, setPatterns] = useState("");
  const [expanded, setExpanded] = useState(true);

  function handleStart() {
    const parsed = patterns
      .split(/[\n,]/)
      .map((s) => s.trim())
      .filter(Boolean);
    if (parsed.length === 0) return;
    start(repo.repo_id, parsed);
  }

  const isRunning = state.phase === "running";
  const isDone = state.phase === "done";
  const isError = state.phase === "error";

  return (
    <div className="flex flex-col gap-3 p-4">
      {/* Header */}
      <div className="flex items-center justify-between">
        <button
          type="button"
          className="flex items-center gap-1 text-sm font-semibold text-gx-text"
          onClick={() => setExpanded(!expanded)}
        >
          {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
          <Package size={14} />
          Track with Crab
        </button>
        {isDone && (
          <button type="button" className="btn text-xs" onClick={() => { reset(); onComplete?.(); }}>
            Done
          </button>
        )}
      </div>

      {expanded && (
        <>
          {/* Pattern input */}
          {state.phase === "idle" && (
            <div className="space-y-2">
              <label className="block text-xs text-gx-muted">
                File patterns (one per line, e.g. <code className="kbd">*.safetensors</code>)
              </label>
              <textarea
                className="h-20 w-full rounded border border-gx-border bg-gx-bg-sub p-2 font-mono text-sm focus-visible:outline-none focus:border-gx-accent"
                value={patterns}
                onChange={(e) => setPatterns(e.target.value)}
                placeholder="*.safetensors&#10;models/**/*.bin&#10;data/*.parquet"
              />
              <button
                type="button"
                className="btn btn-primary text-xs"
                onClick={handleStart}
                disabled={!patterns.trim()}
              >
                <Zap size={12} /> Track & Stage
              </button>
            </div>
          )}

          {/* Running state */}
          {isRunning && (
            <div className="space-y-3">
              {/* Phase indicator */}
              <div className="flex items-center gap-2 text-xs">
                <Loader2 size={12} className="animate-spin text-gx-accent" />
                <span className="font-medium text-gx-text capitalize">
                  {state.currentPhase}
                </span>
                <span className="text-gx-muted">
                  {state.bytesDone > 0 && state.bytesTotal > 0
                    ? `${formatBytes(state.bytesDone)} / ${formatBytes(state.bytesTotal)}`
                    : "Starting..."}
                </span>
              </div>

              {/* Overall progress bar */}
              {state.bytesTotal > 0 && (
                <div className="h-2 w-full overflow-hidden rounded-full bg-gx-border">
                  <div
                    className="h-full rounded-full bg-gx-accent transition-all duration-300"
                    style={{
                      width: `${Math.min(100, (state.bytesDone / state.bytesTotal) * 100)}%`,
                    }}
                  />
                </div>
              )}

              {/* Per-file list */}
              <div className="max-h-48 space-y-1 overflow-auto">
                {state.files.map((f) => (
                  <FileRow key={f.path} file={f} />
                ))}
              </div>

              {/* Cancel button */}
              <button type="button" className="btn text-xs" onClick={cancel}>
                Cancel
              </button>
            </div>
          )}

          {/* Done state */}
          {isDone && (
            <div className="space-y-3">
              <div className="rounded border border-gx-success/30 bg-gx-success/5 p-3">
                <div className="flex items-center gap-2 text-sm font-medium text-gx-success">
                  <CheckCircle2 size={14} />
                  Staging complete
                </div>
                <div className="mt-2 grid grid-cols-2 gap-2 text-xs text-gx-muted">
                  <div>
                    <span className="text-gx-text">{state.summary.files_staged}</span> files
                  </div>
                  <div>
                    <span className="text-gx-text">{state.summary.chunks_staged.toLocaleString()}</span> chunks
                  </div>
                  <div>
                    <span className="text-gx-text">{formatBytes(state.summary.bytes_processed)}</span> processed
                  </div>
                  <div>
                    <span className="text-gx-text">{formatDuration(state.summary.duration_ms)}</span>
                  </div>
                </div>
              </div>

              {/* Per-file results */}
              <div className="max-h-48 space-y-1 overflow-auto">
                {state.files.map((f) => (
                  <FileRow key={f.path} file={f} />
                ))}
              </div>
            </div>
          )}

          {/* Error state */}
          {isError && (
            <div className="space-y-2">
              <div className="rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
                <div className="flex items-center gap-2 font-medium">
                  <XCircle size={12} />
                  {state.message}
                </div>
              </div>
              <button type="button" className="btn text-xs" onClick={reset}>
                Try Again
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}

// ── File row ────────────────────────────────────────────────────────

function FileRow({ file }: { file: import("../ipc").AddFlowFileProgress }) {
  const pct =
    file.size > 0 ? Math.min(100, (file.bytes_done / file.size) * 100) : 0;

  return (
    <div className="flex items-center gap-2 rounded px-2 py-1 text-xs hover:bg-gx-surface-hover">
      {/* Status icon */}
      {file.status === "chunking" && (
        <Loader2 size={10} className="shrink-0 animate-spin text-gx-accent" />
      )}
      {file.status === "done" && (
        <CheckCircle2 size={10} className="shrink-0 text-gx-success" />
      )}
      {file.status === "error" && (
        <XCircle size={10} className="shrink-0 text-gx-error" />
      )}
      {file.status === "pending" && (
        <div className="h-[10px] w-[10px] shrink-0 rounded-full border border-gx-border" />
      )}

      {/* File name */}
      <span className="min-w-0 flex-1 truncate font-mono text-gx-text">
        {file.path.split("/").pop()}
      </span>

      {/* Size */}
      <span className="shrink-0 text-gx-muted">{formatBytes(file.size)}</span>

      {/* Chunks badge */}
      {file.chunks !== null && (
        <span className="shrink-0 rounded bg-gx-surface px-1 text-gx-muted">
          {file.chunks} chunks
        </span>
      )}

      {/* Dedup ratio badge */}
      {file.dedup_ratio !== null && file.dedup_ratio > 1 && (
        <span
          className={clsx(
            "shrink-0 rounded px-1 font-medium",
            file.dedup_ratio >= 2
              ? "bg-gx-success/10 text-gx-success"
              : "bg-gx-warn/10 text-gx-warn",
          )}
        >
          {file.dedup_ratio.toFixed(1)}× dedup
        </span>
      )}

      {/* Progress bar (inline, only while chunking) */}
      {file.status === "chunking" && file.size > 0 && (
        <div className="h-1 w-16 shrink-0 overflow-hidden rounded-full bg-gx-border">
          <div
            className="h-full rounded-full bg-gx-accent"
            style={{ width: `${pct}%` }}
          />
        </div>
      )}
    </div>
  );
}
