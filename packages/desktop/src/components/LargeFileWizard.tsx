/**
 * LargeFileWizard — Guided Track → Commit → Push flow for large files.
 *
 * Replaces the disconnected Track/Push UX with a linear wizard that:
 * 1. Shows untracked large files with pattern registration preview
 * 2. Tracks files (crab track + crab add + git add)
 * 3. Commits the pointer files with an inline commit composer
 * 4. Pushes xorbs to the remote via the 14-step pipeline
 *
 * Each step unlocks the next, showing clear status badges and progress.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import clsx from "clsx";
import {
  AlertTriangle,
  ArrowRight,
  Check,
  CheckCircle2,
  GitCommitHorizontal,
  HardDrive,
  Loader2,
  Package,
  XCircle,
} from "lucide-react";
import { OpenRepo } from "../state";
import { rpc, TrackAndAddResult } from "../ipc";
import { formatBytes } from "../lib/format";
import { PushFlowPanel } from "../views/PushFlowPanel";

interface Props {
  repo: OpenRepo;
  /** Files to track — passed from the parent FilesPanel selection. */
  files: Array<{ path: string; size: number }>;
  /** Called when the wizard completes or is dismissed. */
  onComplete: () => void;
  /** Called to go back to file selection. */
  onBack: () => void;
}

type WizardStep = "review" | "tracking" | "commit" | "push" | "done";

interface FileStatus {
  path: string;
  size: number;
  status: "pending" | "tracking" | "tracked" | "error";
  error?: string;
}

/** Derive unique extension patterns from a list of file paths. */
function derivePatterns(paths: string[]): string[] {
  const exts = new Set<string>();
  for (const p of paths) {
    const ext = p.split(".").pop()?.toLowerCase();
    if (ext) exts.add(`*.${ext}`);
  }
  return Array.from(exts).sort();
}

export function LargeFileWizard({ repo, files, onComplete, onBack }: Props) {
  const [step, setStep] = useState<WizardStep>("review");
  const [fileStatuses, setFileStatuses] = useState<FileStatus[]>(() =>
    files.map((f) => ({ path: f.path, size: f.size, status: "pending" })),
  );
  const [trackResult, setTrackResult] = useState<TrackAndAddResult | null>(null);
  const [commitMessage, setCommitMessage] = useState("");
  const [committing, setCommitting] = useState(false);
  const [commitSha, setCommitSha] = useState<string | null>(null);
  const [commitError, setCommitError] = useState<string | null>(null);

  // Derive patterns that will be registered.
  const patterns = useMemo(() => derivePatterns(files.map((f) => f.path)), [files]);

  // Auto-generate commit message.
  useEffect(() => {
    const names = files.map((f) => f.path.split("/").pop() ?? f.path);
    if (names.length === 1) {
      setCommitMessage(`Track large file: ${names[0]}`);
    } else if (names.length <= 4) {
      setCommitMessage(`Track ${names.length} large files: ${names.join(", ")}`);
    } else {
      setCommitMessage(`Track ${names.length} large files`);
    }
  }, [files]);

  const totalSize = useMemo(() => files.reduce((s, f) => s + f.size, 0), [files]);

  // Step 1 → 2: Start tracking.
  const handleTrack = useCallback(async () => {
    setStep("tracking");
    setFileStatuses((prev) => prev.map((f) => ({ ...f, status: "tracking" })));

    try {
      const result = await rpc.crabAddAuto(
        repo.repo_id,
        files.map((f) => f.path),
      );

      // crabAddAuto returns { ok, tracked, patterns_added?, errors? }
      const resultAny = result as Record<string, unknown>;
      const errors = (resultAny.errors as Array<{ path: string; stderr?: string }>) ?? [];
      const patternsAdded = (resultAny.patterns_added as string[]) ?? [];

      // Build a TrackAndAddResult-compatible shape for the UI.
      const errorPaths = new Set(errors.map((e) => e.path));
      const trackedFiles = files
        .filter((f) => !errorPaths.has(f.path))
        .map((f) => ({ path: f.path, status: "ok" }));

      setTrackResult({
        ok: true,
        patterns_added: patternsAdded,
        files_tracked: trackedFiles,
        errors,
        commit_sha: null,
      });

      // Update per-file statuses.
      setFileStatuses((prev) =>
        prev.map((f) => {
          const err = errors.find((e) => e.path === f.path);
          if (err) {
            return { ...f, status: "error", error: err.stderr ?? "Unknown error" };
          }
          return { ...f, status: "tracked" };
        }),
      );

      // Move to commit step if any files succeeded.
      if (trackedFiles.length > 0) {
        setStep("commit");
      }
    } catch (e) {
      setFileStatuses((prev) =>
        prev.map((f) => ({
          ...f,
          status: "error",
          error: e instanceof Error ? e.message : String(e),
        })),
      );
    }
  }, [repo.repo_id, files]);

  // Step 2 → 3: Commit.
  const handleCommit = useCallback(async () => {
    if (!commitMessage.trim()) return;
    setCommitting(true);
    setCommitError(null);

    try {
      const result = await rpc.gitCommit(repo.repo_id, {
        message: commitMessage.trim(),
      });
      setCommitSha(result.sha);
      setStep("push");
    } catch (e) {
      setCommitError(e instanceof Error ? e.message : String(e));
    } finally {
      setCommitting(false);
    }
  }, [repo.repo_id, commitMessage]);

  // Step 3 → done: Push completed.
  const handlePushComplete = useCallback(() => {
    setStep("done");
  }, []);

  const trackedCount = fileStatuses.filter((f) => f.status === "tracked").length;
  const errorCount = fileStatuses.filter((f) => f.status === "error").length;

  return (
    <div className="flex h-full flex-col overflow-auto">
      {/* Header with step indicator */}
      <div className="shrink-0 border-b border-gx-border bg-gx-surface px-4 py-3">
        <div className="flex items-center gap-2 text-sm font-semibold text-gx-text">
          <Package size={14} className="text-gx-accent" />
          Track Large Files
        </div>
        <StepIndicator currentStep={step} />
      </div>

      {/* Content area */}
      <div className="min-h-0 flex-1 overflow-auto">
        {/* Step 1: Review */}
        {step === "review" && (
          <div className="space-y-4 p-4">
            {/* Summary card */}
            <div className="rounded-lg border border-gx-border bg-gx-bg-sub p-4">
              <div className="flex items-center justify-between">
                <div>
                  <div className="text-sm font-medium text-gx-text">
                    {files.length} file{files.length > 1 ? "s" : ""} selected
                  </div>
                  <div className="mt-0.5 text-xs text-gx-muted">
                    Total size: {formatBytes(totalSize)}
                  </div>
                </div>
                <button
                  type="button"
                  className="text-xs text-gx-accent hover:underline"
                  onClick={onBack}
                >
                  Change selection
                </button>
              </div>
            </div>

            {/* Patterns to register */}
            <div className="rounded-lg border border-gx-accent/20 bg-gx-accent/5 p-4">
              <div className="flex items-center gap-2 text-xs font-medium text-gx-accent">
                <HardDrive size={12} />
                Patterns to register in .gitattributes
              </div>
              <div className="mt-2 flex flex-wrap gap-1.5">
                {patterns.map((p) => (
                  <span
                    key={p}
                    className="rounded-full bg-gx-accent/10 px-2.5 py-0.5 font-mono text-[11px] text-gx-accent"
                  >
                    {p}
                  </span>
                ))}
              </div>
              <p className="mt-2 text-[10px] text-gx-muted">
                These patterns tell git to route matching files through the crab filter for
                chunking and deduplication.
              </p>
            </div>

            {/* File list */}
            <div>
              <div className="mb-2 text-xs font-medium text-gx-muted">Files to track</div>
              <div className="space-y-1">
                {fileStatuses.map((f) => (
                  <FileRow key={f.path} file={f} />
                ))}
              </div>
            </div>

            {/* What will happen */}
            <div className="rounded-lg border border-gx-border p-4">
              <div className="text-xs font-medium text-gx-text">What will happen</div>
              <ol className="mt-2 space-y-1.5 text-[11px] text-gx-muted">
                <li className="flex items-start gap-2">
                  <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-gx-accent/10 text-[9px] font-bold text-gx-accent">
                    1
                  </span>
                  <span>
                    Register patterns ({patterns.join(", ")}) in{" "}
                    <code className="font-mono text-gx-text">.gitattributes</code>
                  </span>
                </li>
                <li className="flex items-start gap-2">
                  <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-gx-accent/10 text-[9px] font-bold text-gx-accent">
                    2
                  </span>
                  <span>
                    Chunk &amp; deduplicate each file into the staging area (
                    <code className="font-mono text-gx-text">.crab/staging/</code>)
                  </span>
                </li>
                <li className="flex items-start gap-2">
                  <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-gx-accent/10 text-[9px] font-bold text-gx-accent">
                    3
                  </span>
                  <span>Replace working tree files with lightweight pointer blobs in git's index</span>
                </li>
                <li className="flex items-start gap-2">
                  <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-gx-accent/10 text-[9px] font-bold text-gx-accent">
                    4
                  </span>
                  <span>Commit the pointers, then push chunks to the remote</span>
                </li>
              </ol>
            </div>

            {/* Action */}
            <button
              type="button"
              className="btn btn-primary w-full justify-center gap-2 py-2.5 text-sm"
              onClick={handleTrack}
            >
              <Package size={14} />
              Track {files.length} file{files.length > 1 ? "s" : ""}
              <ArrowRight size={14} />
            </button>
          </div>
        )}

        {/* Step 2: Tracking in progress / results */}
        {step === "tracking" && (
          <div className="space-y-4 p-4">
            <div className="flex items-center gap-2 text-sm font-medium text-gx-text">
              {errorCount === 0 && trackedCount === 0 && (
                <>
                  <Loader2 size={14} className="animate-spin text-gx-accent" />
                  Tracking files…
                </>
              )}
              {trackedCount > 0 && errorCount === 0 && (
                <>
                  <CheckCircle2 size={14} className="text-gx-success" />
                  All files tracked successfully
                </>
              )}
              {errorCount > 0 && (
                <>
                  <AlertTriangle size={14} className="text-gx-warn" />
                  {trackedCount} tracked, {errorCount} failed
                </>
              )}
            </div>

            {/* Per-file status */}
            <div className="space-y-1">
              {fileStatuses.map((f) => (
                <FileRow key={f.path} file={f} />
              ))}
            </div>

            {/* Patterns added */}
            {trackResult && trackResult.patterns_added.length > 0 && (
              <div className="rounded border border-gx-success/20 bg-gx-success/5 p-3 text-xs">
                <div className="flex items-center gap-1.5 font-medium text-gx-success">
                  <Check size={11} />
                  Patterns registered
                </div>
                <div className="mt-1.5 flex flex-wrap gap-1">
                  {trackResult.patterns_added.map((p) => (
                    <span
                      key={p}
                      className="rounded-full bg-gx-success/10 px-2 py-0.5 font-mono text-[10px] text-gx-success"
                    >
                      {p}
                    </span>
                  ))}
                </div>
              </div>
            )}

            {/* Error details */}
            {errorCount > 0 && (
              <div className="space-y-2">
                {fileStatuses
                  .filter((f) => f.status === "error")
                  .map((f) => (
                    <div
                      key={f.path}
                      className="rounded border border-gx-error/30 bg-gx-error/5 p-2 text-[11px]"
                    >
                      <div className="font-mono text-gx-error">
                        {f.path.split("/").pop()}
                      </div>
                      {f.error && (
                        <div className="mt-1 text-gx-error/70">{f.error}</div>
                      )}
                    </div>
                  ))}
              </div>
            )}

            {/* Continue to commit (if any files succeeded) */}
            {trackedCount > 0 && (
              <button
                type="button"
                className="btn btn-primary w-full justify-center gap-2 py-2.5 text-sm"
                onClick={() => setStep("commit")}
              >
                <GitCommitHorizontal size={14} />
                Continue to Commit
                <ArrowRight size={14} />
              </button>
            )}
          </div>
        )}

        {/* Step 3: Commit */}
        {step === "commit" && (
          <div className="space-y-4 p-4">
            {/* What's being committed */}
            <div className="rounded-lg border border-gx-border bg-gx-bg-sub p-4">
              <div className="flex items-center gap-2 text-xs font-medium text-gx-text">
                <GitCommitHorizontal size={12} className="text-gx-accent" />
                Ready to commit
              </div>
              <div className="mt-2 space-y-1 text-[11px] text-gx-muted">
                <div className="flex items-center gap-1.5">
                  <Check size={10} className="text-gx-success" />
                  {trackedCount} pointer file{trackedCount > 1 ? "s" : ""} staged
                </div>
                {trackResult && trackResult.patterns_added.length > 0 && (
                  <div className="flex items-center gap-1.5">
                    <Check size={10} className="text-gx-success" />
                    .gitattributes updated ({trackResult.patterns_added.join(", ")})
                  </div>
                )}
                <div className="flex items-center gap-1.5">
                  <HardDrive size={10} className="text-gx-accent" />
                  {formatBytes(totalSize)} chunked in staging area
                </div>
              </div>
            </div>

            {/* Commit message */}
            <div>
              <label
                htmlFor="wizard-commit-msg"
                className="mb-1.5 block text-xs font-medium text-gx-text"
              >
                Commit message
              </label>
              <input
                id="wizard-commit-msg"
                type="text"
                className="w-full rounded border border-gx-border bg-gx-bg-sub px-3 py-2 text-xs text-gx-text placeholder:text-gx-muted focus:border-gx-accent focus-visible:outline-none"
                value={commitMessage}
                onChange={(e) => setCommitMessage(e.target.value)}
                onKeyDown={(e) => {
                  if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
                    e.preventDefault();
                    void handleCommit();
                  }
                }}
                placeholder="Describe what you're tracking…"
                aria-label="Commit message for tracked files"
              />
              <p className="mt-1 text-[10px] text-gx-muted">
                Ctrl+Enter to commit. This creates a git commit with the pointer files.
              </p>
            </div>

            {/* Commit error */}
            {commitError && (
              <div className="rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
                <div className="flex items-center gap-1.5 font-medium">
                  <XCircle size={11} />
                  Commit failed
                </div>
                <div className="mt-1 text-gx-error/80">{commitError}</div>
              </div>
            )}

            {/* Actions */}
            <div className="flex gap-2">
              <button
                type="button"
                className="btn btn-primary flex-1 justify-center gap-2 py-2.5 text-sm"
                disabled={!commitMessage.trim() || committing}
                onClick={handleCommit}
              >
                {committing ? (
                  <Loader2 size={14} className="animate-spin" />
                ) : (
                  <GitCommitHorizontal size={14} />
                )}
                {committing ? "Committing…" : "Commit"}
              </button>
              <button
                type="button"
                className="btn flex-1 justify-center gap-2 py-2.5 text-xs"
                onClick={onComplete}
              >
                Skip — commit later
              </button>
            </div>
          </div>
        )}

        {/* Step 4: Push */}
        {step === "push" && (
          <div className="space-y-4 p-4">
            {/* Commit success banner */}
            {commitSha && (
              <div className="rounded border border-gx-success/30 bg-gx-success/5 p-3">
                <div className="flex items-center gap-2 text-xs font-medium text-gx-success">
                  <CheckCircle2 size={12} />
                  Committed
                  <span className="font-mono text-[10px] text-gx-success/70">
                    {commitSha.slice(0, 7)}
                  </span>
                </div>
              </div>
            )}

            {/* Push panel */}
            <div className="rounded-lg border border-gx-border">
              <PushFlowPanel repo={repo} onComplete={handlePushComplete} />
            </div>

            {/* Skip push option */}
            <button
              type="button"
              className="w-full text-center text-xs text-gx-muted hover:text-gx-text"
              onClick={onComplete}
            >
              Skip push — I'll push later
            </button>
          </div>
        )}

        {/* Step 5: Done */}
        {step === "done" && (
          <div className="flex flex-col items-center justify-center p-8 text-center">
            <div className="flex h-12 w-12 items-center justify-center rounded-full bg-gx-success/10">
              <CheckCircle2 size={24} className="text-gx-success" />
            </div>
            <div className="mt-4 text-sm font-medium text-gx-text">All done</div>
            <div className="mt-1 text-xs text-gx-muted">
              {trackedCount} file{trackedCount > 1 ? "s" : ""} tracked, committed, and pushed
              to remote.
            </div>
            <button
              type="button"
              className="btn btn-primary mt-6 gap-2"
              onClick={onComplete}
            >
              <Check size={12} />
              Done
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

// ── Step indicator ──────────────────────────────────────────────────

const STEPS: Array<{ key: WizardStep; label: string }> = [
  { key: "review", label: "Review" },
  { key: "tracking", label: "Track" },
  { key: "commit", label: "Commit" },
  { key: "push", label: "Push" },
  { key: "done", label: "Done" },
];

function StepIndicator({ currentStep }: { currentStep: WizardStep }) {
  const currentIndex = STEPS.findIndex((s) => s.key === currentStep);

  return (
    <div className="mt-2 flex items-center gap-1">
      {STEPS.map((s, i) => {
        const isActive = i === currentIndex;
        const isDone = i < currentIndex;
        return (
          <div key={s.key} className="flex items-center gap-1">
            {i > 0 && (
              <div
                className={clsx(
                  "h-px w-4",
                  isDone ? "bg-gx-success" : "bg-gx-border",
                )}
              />
            )}
            <div
              className={clsx(
                "flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px] font-medium",
                isActive && "bg-gx-accent/10 text-gx-accent",
                isDone && "bg-gx-success/10 text-gx-success",
                !isActive && !isDone && "text-gx-muted",
              )}
            >
              {isDone && <Check size={9} />}
              {s.label}
            </div>
          </div>
        );
      })}
    </div>
  );
}

// ── File row ────────────────────────────────────────────────────────

function FileRow({ file }: { file: FileStatus }) {
  const fileName = file.path.split("/").pop() ?? file.path;
  const dirPath = file.path.includes("/")
    ? file.path.slice(0, file.path.lastIndexOf("/") + 1)
    : "";

  return (
    <div className="flex items-center gap-2 rounded px-3 py-1.5 text-xs">
      {/* Status icon */}
      {file.status === "pending" && (
        <div className="h-3 w-3 shrink-0 rounded-full border border-gx-border" />
      )}
      {file.status === "tracking" && (
        <Loader2 size={12} className="shrink-0 animate-spin text-gx-accent" />
      )}
      {file.status === "tracked" && (
        <CheckCircle2 size={12} className="shrink-0 text-gx-success" />
      )}
      {file.status === "error" && (
        <XCircle size={12} className="shrink-0 text-gx-error" />
      )}

      {/* File name */}
      <div className="flex min-w-0 flex-1 items-baseline gap-1">
        <span className="truncate font-mono text-gx-text">{fileName}</span>
        {dirPath && (
          <span className="truncate text-[10px] text-gx-muted">{dirPath}</span>
        )}
      </div>

      {/* Size */}
      <span className="shrink-0 tabular-nums text-gx-muted">
        {formatBytes(file.size)}
      </span>

      {/* Status badge */}
      <span
        className={clsx(
          "shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium",
          file.status === "pending" && "bg-gx-border/50 text-gx-muted",
          file.status === "tracking" && "bg-gx-accent/10 text-gx-accent",
          file.status === "tracked" && "bg-gx-success/10 text-gx-success",
          file.status === "error" && "bg-gx-error/10 text-gx-error",
        )}
      >
        {file.status === "pending" && "pending"}
        {file.status === "tracking" && "chunking…"}
        {file.status === "tracked" && "tracked"}
        {file.status === "error" && "failed"}
      </span>
    </div>
  );
}
