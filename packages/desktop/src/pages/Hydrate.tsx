import { Button } from "@/components/ui/button";
import { useMemo, useState } from "react";
import { Download, Upload } from "lucide-react";
import { OpenRepo, useStore } from "../state";
import { nextJobId } from "../ipc";
import { useJobLines } from "../lib/useJobLines";
import { useHydrationStatus } from "../hooks/useHydrationStatus";
import { useTrackedPatterns } from "../hooks/useTrackedPatterns";
import { ConfirmHydrateDialog } from "../components/ConfirmHydrateDialog";
import { HydrateScopePicker } from "../components/hydrate/HydrateScopePicker";
import { HydrateScopePreview } from "../components/hydrate/HydrateScopePreview";
import { formatBytes } from "../lib/format";
import { HydrateVerb, Scope, requiresTransferConfirmation, resolveScope } from "../lib/hydrateScope";

interface Selection {
  verb: HydrateVerb;
  paths: string[];
  bytes: number;
  nativeSessionId: string;
}

export function HydratePage({ repo }: { repo: OpenRepo }) {
  const [scope, setScope] = useState<Scope>({ kind: "all" });
  const [jobId, setJobId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmation, setConfirmation] = useState<Selection | null>(null);
  const { runFileTransfer } = useStore();
  const lines = useJobLines(jobId);

  const enabled = !!repo.path || repo.is_remote;
  const canHydrate = !!repo.native_session_id && !!repo.capabilities?.canHydrateSelection;
  const canDehydrate = !!repo.native_session_id && !!repo.capabilities?.canDehydrateSelection;
  const { byPath, summary, loading: loadingStatus, error: statusError } = useHydrationStatus(repo.repo_id, enabled);
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

  async function runNow({ verb, paths, nativeSessionId }: Selection) {
    const id = nextJobId(verb);
    setJobId(id);
    setBusy(true);
    setError(null);
    try {
      await runFileTransfer(repo.repo_id, { paths, verb }, nativeSessionId, id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
      setConfirmation(null);
    }
  }

  /**
   * Decide whether to show the confirm dialog or kick off immediately.
   * Hydrate above the threshold always confirms (multi-GB downloads
   * are not something we want to launch on a single click). Dehydrate
   * always confirms — destructive in spirit, even if reversible.
   */
  function requestRun(verb: HydrateVerb) {
    const nativeSessionId = repo.native_session_id;
    if (!nativeSessionId || (verb === "hydrate" ? !canHydrate : !canDehydrate)) return;
    const resolved = verb === "hydrate" ? resolvedHydrate : resolvedDehydrate;
    if (resolved.matched.length === 0) return;
    const selection = { verb, paths: resolved.matched.map(entry => entry.path), bytes: resolved.bytes, nativeSessionId };
    const heavy = requiresTransferConfirmation(verb, resolved.matched.length, resolved.bytes);
    if (heavy) {
      setConfirmation(selection);
    } else {
      void runNow(selection);
    }
  }

  if (!enabled) {
    return (
      <div className="p-4 text-sm text-muted-foreground">
        Hydration requires a repository with a working tree.
      </div>
    );
  }

  // Friendlier empty state when no files in the repo are tracked by
  // crab. Without this the user sees a useless picker with all-zero
  // counts and disabled buttons.
  const noTrackedFilesYet =
    entries.length === 0 && tracked.patterns.length === 0 && !tracked.loading && !loadingStatus && !statusError;

  return (
    <div className="flex h-full flex-col">
      <div className="space-y-3 border-b border-border bg-background p-4">
        {summary && (
          <div className="rounded border border-border bg-background p-3 text-xs text-muted-foreground">
            <span className="text-foreground">{summary.hydrated.toLocaleString()}</span> hydrated ·{" "}
            <span className="text-foreground">{summary.pointer.toLocaleString()}</span> pointer-only ·{" "}
            <span className="text-warning">{summary.partial}</span> partial ·{" "}
            <span className="text-destructive">{summary.missing}</span> missing
          </div>
        )}

        {noTrackedFilesYet ? (
          <div className="rounded border border-border bg-background p-4 text-xs text-muted-foreground">
            <p className="mb-1 text-foreground">No Crab or Git LFS files in this repository yet.</p>
            <p>
              Track large files via the <strong className="text-foreground">Track &amp; Stage</strong>{" "}
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
              <Button variant="default"
                type="button"
                className="text-xs"
                disabled={busy || !canHydrate || resolvedHydrate.matched.length === 0}
                onClick={() => requestRun("hydrate")}
                title={
                  resolvedHydrate.matched.length === 0
                    ? "Nothing in scope needs hydrating"
                    : `Hydrate ${resolvedHydrate.matched.length} file${resolvedHydrate.matched.length === 1 ? "" : "s"}`
                }
              >
                <Download size={12} />
                Hydrate {formatActionLabel(resolvedHydrate.matched.length, resolvedHydrate.bytes)}
              </Button>
              <Button variant="outline"
                type="button"
                className="text-xs"
                disabled={busy || !canDehydrate || resolvedDehydrate.matched.length === 0}
                onClick={() => requestRun("dehydrate")}
                title={
                  resolvedDehydrate.matched.length === 0
                    ? "Nothing in scope is currently hydrated"
                    : `Dehydrate ${resolvedDehydrate.matched.length} file${resolvedDehydrate.matched.length === 1 ? "" : "s"}`
                }
              >
                <Upload size={12} />
                Dehydrate {formatActionLabel(resolvedDehydrate.matched.length, resolvedDehydrate.bytes)}
              </Button>
            </div>
          </>
        )}

        {(error || statusError) && (
          <div role="alert" className="rounded border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive">
            {error || statusError}
          </div>
        )}
      </div>

      <div className="min-h-0 flex-1 overflow-auto bg-background p-3 font-mono text-[12px]">
        {lines.length === 0 ? (
          <div className="text-muted-foreground">Progress will stream here once the job starts.</div>
        ) : (
          lines.map((l, i) => (
            <div key={i} className={l.stream === "stderr" ? "text-warning" : ""}>
              {l.line}
            </div>
          ))
        )}
      </div>

      <ConfirmHydrateDialog
        open={confirmation !== null}
        verb={confirmation?.verb ?? "hydrate"}
        fileCount={confirmation?.paths.length ?? 0}
        totalBytes={confirmation?.bytes ?? 0}
        busy={busy}
        error={error}
        onCancel={() => {
          if (!busy) setConfirmation(null);
        }}
        onConfirm={() => {
          if (confirmation) void runNow(confirmation);
        }}
      />
    </div>
  );
}

function formatActionLabel(count: number, bytes: number): string {
  if (count === 0) return "(0 files)";
  return `${count.toLocaleString()} ${count === 1 ? "file" : "files"} (${formatBytes(bytes)})`;
}
