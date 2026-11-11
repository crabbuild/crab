import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
/** Guided tracking, selected-file commit and push through the existing owners. */
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
      const result = await rpc.crabTrackAndAdd(repo.repo_id, files.map((file) => file.path));
      const errors = result.errors;
      const trackedPaths = new Set(result.files_tracked.map((file) => file.path));
      setTrackResult(result);

      // Update per-file statuses.
      setFileStatuses((prev) =>
        prev.map((f) => {
          const err = errors.find((e) => e.path === f.path);
          if (err) {
            return { ...f, status: "error", error: err.stderr ?? "Unknown error" };
          }
          return { ...f, status: trackedPaths.has(f.path) ? "tracked" : "error" };
        }),
      );

      // Move to commit step if any files succeeded.
      if (errors.length === 0 && trackedPaths.size > 0) {
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
    if (!commitMessage.trim() || committing || !trackResult?.files_tracked.length) return;
    if (!trackResult.commit_paths?.length) {
      setCommitError("The tracking service returned no commit selection. Retry tracking with the current desktop agent.");
      return;
    }
    setCommitting(true);
    setCommitError(null);

    try {
      const result = await rpc.gitCommit(repo.repo_id, {
        message: commitMessage.trim(),
        paths: trackResult.commit_paths,
      });
      setCommitSha(result.sha);
      setStep("push");
    } catch (e) {
      setCommitError(e instanceof Error ? e.message : String(e));
    } finally {
      setCommitting(false);
    }
  }, [repo.repo_id, commitMessage, committing, trackResult]);

  // Step 3 → done: Push completed.
  const handlePushComplete = useCallback(() => {
    setStep("done");
  }, []);

  const trackedCount = fileStatuses.filter((f) => f.status === "tracked").length;
  const errorCount = fileStatuses.filter((f) => f.status === "error").length;

  return (
    <div className="flex h-full flex-col overflow-auto">
      {/* Header with step indicator */}
      <div className="shrink-0 border-b border-border bg-card px-4 py-3">
        <div className="flex items-center gap-2 text-sm font-semibold text-foreground">
          <Package size={14} className="text-primary" />
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
            <div className="rounded-lg border border-border bg-background p-4">
              <div className="flex items-center justify-between">
                <div>
                  <div className="text-sm font-medium text-foreground">
                    {files.length} file{files.length > 1 ? "s" : ""} selected
                  </div>
                  <div className="mt-0.5 text-xs text-muted-foreground">
                    Total size: {formatBytes(totalSize)}
                  </div>
                </div>
                <button
                  type="button"
                  className="text-xs text-primary hover:underline"
                  onClick={onBack}
                >
                  Change selection
                </button>
              </div>
            </div>

            {/* Patterns to register */}
            <div className="rounded-lg border border-primary/20 bg-primary/5 p-4">
              <div className="flex items-center gap-2 text-xs font-medium text-primary">
                <HardDrive size={12} />
                Selected paths
              </div>
              <div className="mt-2 flex flex-wrap gap-1.5">
                {files.map(({ path: p }) => (
                  <span
                    key={p}
                    className="rounded-full bg-primary/10 px-2.5 py-0.5 font-mono text-[11px] text-primary"
                  >
                    {p}
                  </span>
                ))}
              </div>
              <p className="mt-2 text-[10px] text-muted-foreground">
                Tracking configures repository-local Crab and Git LFS drivers with Crab. New paths use Crab chunking. Existing file routing is retained.
              </p>
            </div>

            {/* File list */}
            <div>
              <div className="mb-2 text-xs font-medium text-muted-foreground">Files to track</div>
              <div className="space-y-1">
                {fileStatuses.map((f) => (
                  <FileRow key={f.path} file={f} />
                ))}
              </div>
            </div>

            {/* What will happen */}
            <div className="rounded-lg border border-border p-4">
              <div className="text-xs font-medium text-foreground">What will happen</div>
              <ol className="mt-2 space-y-1.5 text-[11px] text-muted-foreground">
                <li className="flex items-start gap-2">
                  <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-primary/10 text-[9px] font-bold text-primary">
                    1
                  </span>
                  <span>
                    Register selected paths in{" "}
                    <code className="font-mono text-foreground">.gitattributes</code>
                  </span>
                </li>
                <li className="flex items-start gap-2">
                  <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-primary/10 text-[9px] font-bold text-primary">
                    2
                  </span>
                  <span>
                    Store each file through its configured filter
                  </span>
                </li>
                <li className="flex items-start gap-2">
                  <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-primary/10 text-[9px] font-bold text-primary">
                    3
                  </span>
                  <span>Stage lightweight pointers while keeping the working files</span>
                </li>
                <li className="flex items-start gap-2">
                  <span className="mt-0.5 flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-primary/10 text-[9px] font-bold text-primary">
                    4
                  </span>
                  <span>Commit the selected files and tracking rules, then push to the remote</span>
                </li>
              </ol>
            </div>

            {/* Action */}
            <Button variant="default"
              type="button"
              className="w-full justify-center gap-2 py-2.5 text-sm"
              onClick={handleTrack}
            >
              <Package size={14} />
              Track {files.length} file{files.length > 1 ? "s" : ""}
              <ArrowRight size={14} />
            </Button>
          </div>
        )}

        {/* Step 2: Tracking in progress / results */}
        {step === "tracking" && (
          <div className="space-y-4 p-4">
            <div className="flex items-center gap-2 text-sm font-medium text-foreground">
              {errorCount === 0 && trackedCount === 0 && (
                <>
                  <Loader2 size={14} className="animate-spin text-primary" />
                  Tracking files…
                </>
              )}
              {trackedCount > 0 && errorCount === 0 && (
                <>
                  <CheckCircle2 size={14} className="text-success" />
                  All files tracked successfully
                </>
              )}
              {errorCount > 0 && (
                <>
                  <AlertTriangle size={14} className="text-warning" />
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
              <div className="rounded border border-success/20 bg-success/5 p-3 text-xs">
                <div className="flex items-center gap-1.5 font-medium text-success">
                  <Check size={11} />
                  Patterns registered
                </div>
                <div className="mt-1.5 flex flex-wrap gap-1">
                  {trackResult.patterns_added.map((p) => (
                    <span
                      key={p}
                      className="rounded-full bg-success/10 px-2 py-0.5 font-mono text-[10px] text-success"
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
                      className="rounded border border-destructive/30 bg-destructive/5 p-2 text-[11px]"
                    >
                      <div className="font-mono text-destructive">
                        {f.path.split("/").pop()}
                      </div>
                      {f.error && (
                        <div className="mt-1 text-destructive/70">{f.error}</div>
                      )}
                    </div>
                  ))}
              </div>
            )}

            {errorCount > 0 && <Button variant="outline" type="button" className="w-full justify-center" onClick={handleTrack}>Retry tracking</Button>}

            {/* Continue to commit (if any files succeeded) */}
            {trackedCount > 0 && (
              <Button variant="default"
                type="button"
                className="w-full justify-center gap-2 py-2.5 text-sm"
                onClick={() => setStep("commit")}
              >
                <GitCommitHorizontal size={14} />
                Continue to Commit
                <ArrowRight size={14} />
              </Button>
            )}
          </div>
        )}

        {/* Step 3: Commit */}
        {step === "commit" && (
          <div className="space-y-4 p-4">
            {/* What's being committed */}
            <div className="rounded-lg border border-border bg-background p-4">
              <div className="flex items-center gap-2 text-xs font-medium text-foreground">
                <GitCommitHorizontal size={12} className="text-primary" />
                Ready to commit
              </div>
              <div className="mt-2 space-y-1 text-[11px] text-muted-foreground">
                <div className="flex items-center gap-1.5">
                  <Check size={10} className="text-success" />
                  {trackedCount} pointer file{trackedCount > 1 ? "s" : ""} staged
                </div>
                {trackResult?.commit_paths.includes(".gitattributes") && (
                  <div className="flex items-center gap-1.5">
                    <Check size={10} className="text-success" />
                    Tracking rules in .gitattributes
                  </div>
                )}
                <div className="flex items-center gap-1.5">
                  <HardDrive size={10} className="text-primary" />
                  {formatBytes(fileStatuses.filter(file => file.status === "tracked").reduce((sum, file) => sum + file.size, 0))} tracked
                </div>
              </div>
            </div>

            {/* Commit message */}
            <div>
              <label
                htmlFor="wizard-commit-msg"
                className="mb-1.5 block text-xs font-medium text-foreground"
              >
                Commit message
              </label>
              <Input
                id="wizard-commit-msg"
                type="text"
                className="w-full rounded border border-border bg-background px-3 py-2 text-xs text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
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
              <p className="mt-1 text-[10px] text-muted-foreground">
                Ctrl+Enter commits the current contents of these selected files and their tracking rules.
              </p>
            </div>

            {/* Commit error */}
            {commitError && (
              <div className="rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
                <div className="flex items-center gap-1.5 font-medium">
                  <XCircle size={11} />
                  Commit failed
                </div>
                <div className="mt-1 text-destructive/80">{commitError}</div>
              </div>
            )}

            {/* Actions */}
            <div className="flex gap-2">
              <Button variant="default"
                type="button"
                className="flex-1 justify-center gap-2 py-2.5 text-sm"
                disabled={!commitMessage.trim() || committing}
                onClick={handleCommit}
              >
                {committing ? (
                  <Loader2 size={14} className="animate-spin" />
                ) : (
                  <GitCommitHorizontal size={14} />
                )}
                {committing ? "Committing…" : "Commit"}
              </Button>
              <Button variant="outline"
                type="button"
                className="flex-1 justify-center gap-2 py-2.5 text-xs"
                onClick={onComplete}
              >
                Skip — commit later
              </Button>
            </div>
          </div>
        )}

        {/* Step 4: Push */}
        {step === "push" && (
          <div className="space-y-4 p-4">
            {/* Commit success banner */}
            {commitSha && (
              <div className="rounded border border-success/30 bg-success/5 p-3">
                <div className="flex items-center gap-2 text-xs font-medium text-success">
                  <CheckCircle2 size={12} />
                  Committed
                  <span className="font-mono text-[10px] text-success/70">
                    {commitSha.slice(0, 7)}
                  </span>
                </div>
              </div>
            )}

            {/* Push panel */}
            <div className="rounded-lg border border-border">
              <PushFlowPanel repo={repo} onComplete={handlePushComplete} />
            </div>

            {/* Skip push option */}
            <button
              type="button"
              className="w-full text-center text-xs text-muted-foreground hover:text-foreground"
              onClick={onComplete}
            >
              Skip push — I'll push later
            </button>
          </div>
        )}

        {/* Step 5: Done */}
        {step === "done" && (
          <div className="flex flex-col items-center justify-center p-8 text-center">
            <div className="flex h-12 w-12 items-center justify-center rounded-full bg-success/10">
              <CheckCircle2 size={24} className="text-success" />
            </div>
            <div className="mt-4 text-sm font-medium text-foreground">All done</div>
            <div className="mt-1 text-xs text-muted-foreground">
              {trackedCount} file{trackedCount > 1 ? "s" : ""} tracked, committed, and pushed
              to remote.
            </div>
            <Button variant="default"
              type="button"
              className="mt-6 gap-2"
              onClick={onComplete}
            >
              <Check size={12} />
              Done
            </Button>
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
                  isDone ? "bg-success" : "bg-border",
                )}
              />
            )}
            <div
              className={clsx(
                "flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px] font-medium",
                isActive && "bg-primary/10 text-primary",
                isDone && "bg-success/10 text-success",
                !isActive && !isDone && "text-muted-foreground",
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
        <div className="h-3 w-3 shrink-0 rounded-full border border-border" />
      )}
      {file.status === "tracking" && (
        <Loader2 size={12} className="shrink-0 animate-spin text-primary" />
      )}
      {file.status === "tracked" && (
        <CheckCircle2 size={12} className="shrink-0 text-success" />
      )}
      {file.status === "error" && (
        <XCircle size={12} className="shrink-0 text-destructive" />
      )}

      {/* File name */}
      <div className="flex min-w-0 flex-1 items-baseline gap-1">
        <span className="truncate font-mono text-foreground">{fileName}</span>
        {dirPath && (
          <span className="truncate text-[10px] text-muted-foreground">{dirPath}</span>
        )}
      </div>

      {/* Size */}
      <span className="shrink-0 tabular-nums text-muted-foreground">
        {formatBytes(file.size)}
      </span>

      {/* Status badge */}
      <span
        className={clsx(
          "shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium",
          file.status === "pending" && "bg-border/50 text-muted-foreground",
          file.status === "tracking" && "bg-primary/10 text-primary",
          file.status === "tracked" && "bg-success/10 text-success",
          file.status === "error" && "bg-destructive/10 text-destructive",
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
