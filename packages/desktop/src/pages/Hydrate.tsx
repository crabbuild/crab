import { useMemo, useState } from "react";
import { Download, Upload } from "lucide-react";
import { OpenRepo, useStore } from "../state";
import { nextJobId, rpc } from "../ipc";
import { useJobLines } from "../lib/useJobLines";
import { useHydrationStatus } from "../hooks/useHydrationStatus";
import { useTrackedPatterns } from "../hooks/useTrackedPatterns";
import { ConfirmHydrateDialog } from "../components/ConfirmHydrateDialog";
import { HydrateScopePicker } from "../components/hydrate/HydrateScopePicker";
import { HydrateScopePreview } from "../components/hydrate/HydrateScopePreview";
import { formatBytes } from "../lib/format";
import { HydrateVerb, Scope, resolveScope } from "../lib/hydrateScope";

/** Auto-confirm threshold for hydrate (above this we always show the dialog). */
const CONFIRM_BYTES_THRESHOLD = 1 * 1024 * 1024 * 1024;
const CONFIRM_FILES_THRESHOLD = 100;

export function HydratePage({ repo }: { repo: OpenRepo }) {
  const [scope, setScope] = useState<Scope>({ kind: "all" });
  const [jobId, setJobId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmVerb, setConfirmVerb] = useState<HydrateVerb | null>(null);
  const { startJob } = useStore();
  const lines = useJobLines(jobId);

  const enabled = !!repo.path;
  const { byPath, refresh, summary } = useHydrationStatus(repo.repo_id, enabled);
  const tracked = useTrackedPatterns(repo, enabled);

  const entries = useMemo(() => Array.from(byPath.values()), [byPath]);

  // Resolve the scope twice (for hydrate + dehydrate) so each button
  // can show its own count without re-running on click.
  const resolvedHydrate = useMemo(
    () => resolveScope(scope, entries, "hydrate"),
    [scope, entries],
  );
  const resolvedDehydrate = useMemo(
    () => resolveScope(scope, entries, "dehydrate"),
    [scope, entries],
  );

  const activePreview = confirmVerb === "dehydrate" ? resolvedDehydrate : resolvedHydrate;

  async function runNow(verb: HydrateVerb) {
    if (!repo.path) return;
    const resolved = verb === "hydrate" ? resolvedHydrate : resolvedDehydrate;
    if (resolved.matched.length === 0) {
      setConfirmVerb(null);
      return;
    }
    const id = nextJobId(verb);
    startJob(id, verb);
    setJobId(id);
    setBusy(true);
    setError(null);
    try {
      if (verb === "hydrate") await rpc.hydrate(repo.path, resolved.patterns, id);
      else await rpc.dehydrate(repo.path, resolved.patterns, id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
      setConfirmVerb(null);
      refresh();
    }
  }

  /**
   * Decide whether to show the confirm dialog or kick off immediately.
   * Hydrate above the threshold always confirms (multi-GB downloads
   * are not something we want to launch on a single click). Dehydrate
   * always confirms — destructive in spirit, even if reversible.
   */
  function requestRun(verb: HydrateVerb) {
    const resolved = verb === "hydrate" ? resolvedHydrate : resolvedDehydrate;
    if (resolved.matched.length === 0) return;
    const heavy =
      verb === "dehydrate" ||
      resolved.bytes >= CONFIRM_BYTES_THRESHOLD ||
      resolved.matched.length >= CONFIRM_FILES_THRESHOLD;
    if (heavy) {
      setConfirmVerb(verb);
    } else {
      void runNow(verb);
    }
  }

  if (!repo.path) {
    return (
      <div className="p-4 text-sm text-gx-muted">
        Hydrate / dehydrate are only available for local repositories.
      </div>
    );
  }

  // Friendlier empty state when no files in the repo are tracked by
  // crab. Without this the user sees a useless picker with all-zero
  // counts and disabled buttons.
  const noTrackedFilesYet =
    entries.length === 0 && tracked.patterns.length === 0 && !tracked.loading;

  return (
    <div className="flex h-full flex-col">
      <div className="space-y-3 border-b border-gx-border bg-gx-bg p-4">
        {summary && (
          <div className="rounded border border-gx-border bg-gx-bg-sub p-3 text-xs text-gx-muted">
            <span className="text-gx-text">{summary.hydrated.toLocaleString()}</span> hydrated ·{" "}
            <span className="text-gx-text">{summary.pointer.toLocaleString()}</span> pointer-only ·{" "}
            <span className="text-gx-warn">{summary.partial}</span> partial ·{" "}
            <span className="text-gx-error">{summary.missing}</span> missing
          </div>
        )}

        {noTrackedFilesYet ? (
          <div className="rounded border border-gx-border bg-gx-bg-sub p-4 text-xs text-gx-muted">
            <p className="mb-1 text-gx-text">No crab-tracked files in this repo yet.</p>
            <p>
              Track large files via the <strong className="text-gx-text">Track &amp; Stage</strong>{" "}
              tab or by running <code className="kbd">crab track '*.bin'</code>. They'll show up
              here once tracked.
            </p>
          </div>
        ) : (
          <>
            <HydrateScopePicker
              scope={scope}
              onChange={setScope}
              entries={entries}
              trackedPatterns={tracked.patterns}
              profiles={tracked.profiles}
            />

            <HydrateScopePreview
              hydrate={resolvedHydrate}
              dehydrate={resolvedDehydrate}
            />

            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                className="btn btn-primary text-xs"
                disabled={busy || resolvedHydrate.matched.length === 0}
                onClick={() => requestRun("hydrate")}
                title={
                  resolvedHydrate.matched.length === 0
                    ? "Nothing in scope needs hydrating"
                    : `Hydrate ${resolvedHydrate.matched.length} file${resolvedHydrate.matched.length === 1 ? "" : "s"}`
                }
              >
                <Download size={12} />
                Hydrate {formatActionLabel(resolvedHydrate.matched.length, resolvedHydrate.bytes)}
              </button>
              <button
                type="button"
                className="btn text-xs"
                disabled={busy || resolvedDehydrate.matched.length === 0}
                onClick={() => requestRun("dehydrate")}
                title={
                  resolvedDehydrate.matched.length === 0
                    ? "Nothing in scope is currently hydrated"
                    : `Dehydrate ${resolvedDehydrate.matched.length} file${resolvedDehydrate.matched.length === 1 ? "" : "s"}`
                }
              >
                <Upload size={12} />
                Dehydrate {formatActionLabel(resolvedDehydrate.matched.length, resolvedDehydrate.bytes)}
              </button>
            </div>
          </>
        )}

        {error && (
          <div className="rounded border border-gx-error/40 bg-gx-error/10 p-2 text-xs text-gx-error">
            {error}
          </div>
        )}
      </div>

      <div className="min-h-0 flex-1 overflow-auto bg-gx-bg-sub p-3 font-mono text-[12px]">
        {lines.length === 0 ? (
          <div className="text-gx-muted">Progress will stream here once the job starts.</div>
        ) : (
          lines.map((l, i) => (
            <div key={i} className={l.stream === "stderr" ? "text-gx-warn" : ""}>
              {l.line}
            </div>
          ))
        )}
      </div>

      <ConfirmHydrateDialog
        open={confirmVerb !== null}
        verb={confirmVerb ?? "hydrate"}
        fileCount={activePreview.matched.length}
        totalBytes={activePreview.bytes}
        busy={busy}
        error={error}
        onCancel={() => {
          if (!busy) setConfirmVerb(null);
        }}
        onConfirm={() => {
          if (confirmVerb) void runNow(confirmVerb);
        }}
      />
    </div>
  );
}

function formatActionLabel(count: number, bytes: number): string {
  if (count === 0) return "(0 files)";
  return `${count.toLocaleString()} ${count === 1 ? "file" : "files"} (${formatBytes(bytes)})`;
}
