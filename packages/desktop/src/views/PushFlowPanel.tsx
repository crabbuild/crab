import { useEffect, useMemo, useState } from "react";
import clsx from "clsx";
import {
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Cloud,
  Loader2,
  Upload,
  XCircle,
} from "lucide-react";
import { OpenRepo } from "../state";
import { PUSH_STEPS, PushPhaseGroup, useCrabPushFlow } from "../hooks/usePushFlow";
import {
  loadPersistedRemote,
  persistRemote,
  usePushTarget,
} from "../hooks/usePushTarget";
import { formatBytes, formatDuration } from "../lib/format";
import { rpc } from "../ipc";
import { PushTargetCard } from "../components/push/PushTargetCard";
import { ForcePushConfirmDialog } from "../components/push/ForcePushConfirmDialog";

interface Props {
  repo: OpenRepo;
  /** Called after a successful push so the parent can refresh state. */
  onComplete?: () => void;
}

const GROUP_LABELS: Record<PushPhaseGroup, string> = {
  finalize: "Finalize",
};

/**
 * Visual push pipeline panel. Always renders the [`PushTargetCard`] so
 * users can confirm where the push is going *before* they click. Shows
 * a vertical stepper with the 14 push steps grouped into phases,
 * upload progress with throughput, and a summary card on completion
 * that includes the actual `crab://` URL the bytes landed at.
 */
export function PushFlowPanel({ repo, onComplete }: Props) {
  const { state, start, cancel, reset } = useCrabPushFlow();
  const target = usePushTarget(repo.repo_id);
  const [showDetails, setShowDetails] = useState(false);
  const [confirmCancel, setConfirmCancel] = useState(false);
  const [confirmForce, setConfirmForce] = useState(false);
  const [pulling, setPulling] = useState(false);
  const [pullError, setPullError] = useState<string | null>(null);

  // ── Selected remote ────────────────────────────────────────────
  // Preference order: persisted choice → upstream remote → first
  // crab remote. We keep a *user-explicit* `selectedRemote` so the
  // hook can refresh remotes without clobbering the user's pick.
  const [selectedRemote, setSelectedRemote] = useState<string | null>(() =>
    loadPersistedRemote(repo.repo_id),
  );

  // Once remotes load, fall back to upstream's remote or the first
  // crab remote when no persisted choice exists. Re-run when the
  // remote list changes (e.g. user added/removed remotes).
  useEffect(() => {
    if (target.crabRemotes.length === 0) return;
    setSelectedRemote((prev) => {
      // If the persisted choice is still valid, keep it.
      if (prev && target.crabRemotes.some((r) => r.name === prev)) return prev;
      const upstreamRemote =
        target.upstream.remote &&
        target.crabRemotes.find((r) => r.name === target.upstream.remote);
      if (upstreamRemote) return upstreamRemote.name;
      return target.crabRemotes[0].name;
    });
  }, [target.crabRemotes, target.upstream.remote]);

  function chooseRemote(name: string) {
    setSelectedRemote(name);
    persistRemote(repo.repo_id, name);
  }

  // ── Destination branch ────────────────────────────────────────
  // Defaults to the upstream's branch if set, otherwise the local
  // branch name (most-common case for a first push). Stays a string
  // so users can rename mid-flow without restarting state.
  const [destBranch, setDestBranch] = useState<string>("");
  useEffect(() => {
    // Adopt the upstream's branch (or local branch as fallback) the
    // first time we have a real value to show. Don't overwrite later
    // edits the user has made.
    if (destBranch) return;
    const next = target.upstream.branch ?? target.branch ?? "";
    if (next) setDestBranch(next);
  }, [target.upstream.branch, target.branch, destBranch]);

  const selected = useMemo(
    () => target.crabRemotes.find((r) => r.name === selectedRemote) ?? null,
    [target.crabRemotes, selectedRemote],
  );

  const isRunning = state.phase === "running";
  const isDone = state.phase === "done";
  const isError = state.phase === "error";

  // ── Action handlers ────────────────────────────────────────────

  function handleStart(opts?: { force?: boolean }) {
    if (!selectedRemote) return;
    start(repo.repo_id, { ...opts, remote: selectedRemote });
  }

  async function handlePullFirst() {
    if (!target.branch) return;
    setPulling(true);
    setPullError(null);
    try {
      await rpc.gitPullBranch(repo.repo_id, target.branch);
      target.refresh();
    } catch (e) {
      setPullError(e instanceof Error ? e.message : String(e));
    } finally {
      setPulling(false);
    }
  }

  async function handleSetUpstream() {
    if (!target.branch || !selectedRemote || !destBranch) return;
    try {
      await rpc.gitSetUpstream(
        repo.repo_id,
        target.branch,
        `${selectedRemote}/${destBranch}`,
      );
      target.refresh();
    } catch {
      // Non-fatal — surface via target.error if needed; the next push
      // will retry by passing -u to the underlying git push.
    }
  }

  // ── Push readiness ─────────────────────────────────────────────
  // We disable Push when there's nothing useful to do: detached HEAD,
  // no crab remote, or already up-to-date. Force-push is gated through
  // the confirm dialog regardless.

  const noCrabRemote = target.crabRemotes.length === 0;
  const upToDate =
    target.upstream.full !== null &&
    target.upstream.ahead === 0 &&
    target.upstream.remote === selectedRemote &&
    target.upstream.branch === destBranch;
  const canPush = !target.detached && !noCrabRemote && !!selected;

  const pushLabel = (() => {
    if (target.detached) return "Detached HEAD — checkout a branch to push";
    if (noCrabRemote) return "No crab remote configured";
    if (!selected) return "Select a destination";
    if (upToDate) return "Up to date";
    const ahead = target.upstream.ahead;
    if (ahead > 0) {
      return `Push ${ahead} commit${ahead === 1 ? "" : "s"} to ${selected.name}`;
    }
    return `Push to ${selected.name}`;
  })();

  return (
    <div className="flex flex-col gap-3 p-4">
      {/* Header */}
      <div className="flex items-center gap-2 text-sm font-semibold text-gx-text">
        <Cloud size={14} />
        Push to Remote
      </div>

      {/* Always-visible target card */}
      <PushTargetCard
        target={target}
        selectedRemote={selectedRemote}
        onSelectRemote={chooseRemote}
        destinationBranch={destBranch}
        onChangeDestinationBranch={setDestBranch}
        onPullFirst={target.upstream.behind > 0 ? handlePullFirst : undefined}
        onSetUpstream={
          !target.upstream.full && target.branch && selected
            ? handleSetUpstream
            : undefined
        }
      />

      {pulling && (
        <div className="rounded border border-gx-border bg-gx-bg-sub p-2 text-xs text-gx-muted">
          <Loader2 size={12} className="mr-1 inline animate-spin" />
          Pulling from upstream…
        </div>
      )}
      {pullError && (
        <div className="rounded border border-gx-error/40 bg-gx-error/10 p-2 text-xs text-gx-error">
          Pull failed: {pullError}
        </div>
      )}

      {/* Idle: action buttons */}
      {state.phase === "idle" && (
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            className="btn btn-primary text-xs"
            disabled={!canPush || upToDate}
            onClick={() => handleStart()}
            title={pushLabel}
          >
            <Upload size={12} />
            {pushLabel}
          </button>
          <button
            type="button"
            className="btn text-xs"
            disabled={!canPush}
            onClick={() => setConfirmForce(true)}
            title={selected ? `Force push to ${selected.name}` : undefined}
          >
            Force push
          </button>
        </div>
      )}

      {/* Running: stepper + upload progress */}
      {isRunning && (
        <div className="space-y-3">
          {state.uploads.bytes_uploaded > 0 && (
            <div className="space-y-1">
              <div className="flex items-center justify-between text-xs text-gx-muted">
                <span>
                  {formatBytes(state.uploads.bytes_uploaded)}
                  {state.uploads.bytes_total
                    ? ` / ${formatBytes(state.uploads.bytes_total)}`
                    : " uploaded"}
                </span>
                {state.uploads.throughput_bps > 0 && (
                  <span>{formatBytes(state.uploads.throughput_bps)}/s</span>
                )}
              </div>
              <div className="h-2 w-full overflow-hidden rounded-full bg-gx-border">
                <div
                  className="h-full rounded-full bg-gx-accent transition-all duration-300"
                  style={{
                    width: state.uploads.bytes_total
                      ? `${Math.min(100, (state.uploads.bytes_uploaded / state.uploads.bytes_total) * 100)}%`
                      : "50%",
                  }}
                />
              </div>
              {state.uploads.xorbs_done > 0 && (
                <div className="text-xs text-gx-muted">
                  {state.uploads.xorbs_done} xorbs packed
                  {state.uploads.xorbs_total ? ` / ${state.uploads.xorbs_total}` : ""}
                </div>
              )}
            </div>
          )}

          <PipelineStepper steps={state.steps} currentStep={state.currentStep} />

          {!confirmCancel ? (
            <button
              type="button"
              className="btn text-xs"
              onClick={() => setConfirmCancel(true)}
            >
              Cancel Push
            </button>
          ) : (
            <div className="flex flex-wrap items-center gap-2 rounded border border-gx-warn/40 bg-gx-warn/10 p-2 text-xs">
              <span className="text-gx-warn">
                Cancelling mid-push may leave partial state on the remote.
              </span>
              <button
                type="button"
                className="btn text-xs"
                onClick={() => {
                  cancel();
                  setConfirmCancel(false);
                }}
              >
                Confirm Cancel
              </button>
              <button
                type="button"
                className="btn text-xs"
                onClick={() => setConfirmCancel(false)}
              >
                Keep Going
              </button>
            </div>
          )}
        </div>
      )}

      {/* Done: summary with destination URL */}
      {isDone && (
        <div className="space-y-3">
          <div className="rounded border border-gx-success/30 bg-gx-success/5 p-3">
            <div className="flex items-center gap-2 text-sm font-medium text-gx-success">
              <CheckCircle2 size={14} />
              Push complete
            </div>
            <div className="mt-2 grid grid-cols-2 gap-2 text-xs text-gx-muted">
              <div>
                <span className="text-gx-text">{state.summary.refs_pushed}</span> ref
                {state.summary.refs_pushed !== 1 ? "s" : ""} pushed
              </div>
              <div>
                <span className="text-gx-text">
                  {state.summary.bytes_uploaded == null ? "–" : formatBytes(state.summary.bytes_uploaded)}
                </span>{" "}
                uploaded
              </div>
              <div>
                <span className="text-gx-text">{state.summary.xorbs_created ?? "–"}</span> xorbs
              </div>
              <div>
                <span className="text-gx-text">
                  {formatDuration(state.summary.duration_ms)}
                </span>
              </div>
            </div>
            {selected && (
              <div className="mt-2 border-t border-gx-success/20 pt-2 text-xs text-gx-muted">
                Pushed to{" "}
                <code className="font-mono text-gx-text">{selected.name}</code>{" "}
                <span className="break-all font-mono">{selected.url}</span>
              </div>
            )}
          </div>

          <button
            type="button"
            className="flex items-center gap-1 text-xs text-gx-muted hover:text-gx-text"
            onClick={() => setShowDetails(!showDetails)}
          >
            {showDetails ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
            Step timings
          </button>
          {showDetails && (
            <div className="space-y-1 text-xs">
              {state.steps
                .filter((s) => s.duration_ms !== null)
                .map((s) => (
                  <div key={s.index} className="flex justify-between text-gx-muted">
                    <span>{PUSH_STEPS[s.index - 1]?.label ?? s.name}</span>
                    <span className="font-mono">{formatDuration(s.duration_ms!)}</span>
                  </div>
                ))}
            </div>
          )}

          <button
            type="button"
            className="btn text-xs"
            onClick={() => {
              reset();
              target.refresh();
              onComplete?.();
            }}
          >
            Done
          </button>
        </div>
      )}

      {/* Error */}
      {isError && (
        <div className="space-y-2">
          <div className="rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
            <div className="flex items-center gap-2 font-medium">
              <XCircle size={12} />
              {state.message}
            </div>
          </div>
          <div className="flex gap-2">
            <button type="button" className="btn text-xs" onClick={reset}>
              Dismiss
            </button>
            <button
              type="button"
              className="btn btn-primary text-xs"
              disabled={!canPush}
              onClick={() => {
                reset();
                handleStart();
              }}
            >
              Retry
            </button>
          </div>
        </div>
      )}

      {/* Force-push confirmation */}
      {selected && target.branch && (
        <ForcePushConfirmDialog
          open={confirmForce}
          branch={target.branch}
          remoteName={selected.name}
          remoteUrl={selected.url}
          destinationBranch={destBranch || target.branch}
          busy={isRunning}
          onCancel={() => setConfirmForce(false)}
          onConfirm={() => {
            setConfirmForce(false);
            handleStart({ force: true });
          }}
        />
      )}
    </div>
  );
}

// ── Pipeline stepper ────────────────────────────────────────────────

function PipelineStepper({
  steps,
}: {
  steps: import("../ipc").PushStepInfo[];
  currentStep: number;
}) {
  const groups = PUSH_STEPS.reduce(
    (acc, def) => {
      if (!acc[def.group]) acc[def.group] = [];
      acc[def.group].push(def);
      return acc;
    },
    {} as Record<PushPhaseGroup, typeof PUSH_STEPS>,
  );

  return (
    <div className="space-y-2">
      {(Object.entries(groups) as [PushPhaseGroup, typeof PUSH_STEPS][]).map(
        ([group, defs]) => (
          <div key={group}>
            <div className="mb-1 text-[10px] uppercase tracking-wider text-gx-muted">
              {GROUP_LABELS[group]}
            </div>
            <div className="space-y-0.5">
              {defs.map((def) => {
                const step = steps.find((s) => s.index === def.index);
                const status = step?.status ?? "pending";
                return (
                  <div
                    key={def.index}
                    className={clsx(
                      "flex items-center gap-2 rounded px-2 py-0.5 text-xs",
                      status === "running" && "bg-gx-accent/10",
                    )}
                  >
                    <StepIcon status={status} />
                    <span
                      className={clsx(
                        "flex-1",
                        status === "running"
                          ? "font-medium text-gx-text"
                          : status === "done"
                            ? "text-gx-muted"
                            : "text-gx-muted/60",
                      )}
                    >
                      {def.label}
                    </span>
                    {step?.duration_ms !== null && step?.duration_ms !== undefined && (
                      <span className="font-mono text-gx-muted">
                        {formatDuration(step.duration_ms)}
                      </span>
                    )}
                  </div>
                );
              })}
            </div>
          </div>
        ),
      )}
    </div>
  );
}

function StepIcon({ status }: { status: string }) {
  switch (status) {
    case "running":
      return <Loader2 size={10} className="shrink-0 animate-spin text-gx-accent" />;
    case "done":
      return <CheckCircle2 size={10} className="shrink-0 text-gx-success" />;
    case "error":
      return <XCircle size={10} className="shrink-0 text-gx-error" />;
    default:
      return <div className="h-[10px] w-[10px] shrink-0 rounded-full border border-gx-border" />;
  }
}
