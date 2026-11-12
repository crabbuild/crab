import { Button } from "@/components/ui/button";
import { SIDECAR_DEFS, SidecarDef } from "@/lib/sidecar-defs";
import { sidecar, SidecarId, SidecarState } from "@/lib/sidecar-ipc";
import {
  AlertCircle,
  Circle,
  Database,
  ExternalLink,
  FlaskConical,
  LineChart,
  Loader2,
  NotebookPen,
  Play,
  Square,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";

// ── Icon lookup ──────────────────────────────────────────────────────

type LucideIcon = typeof Database;

const ICON_MAP: Record<string, LucideIcon> = {
  "notebook-pen": NotebookPen,
  "line-chart": LineChart,
  "flask-conical": FlaskConical,
  database: Database,
};

function SidecarIcon({ icon, size = 12, className }: { icon: string; size?: number; className?: string }) {
  const Icon = ICON_MAP[icon] ?? Database;
  return <Icon size={size} className={className} />;
}

// ── SidecarChips (StatusBar integration) ─────────────────────────────

/**
 * Compact status chips rendered in the StatusBar. Polls sidecar status
 * every 5 seconds and shows a chip for each sidecar that is running,
 * starting, or in an error state.
 */
export function SidecarChips() {
  const [states, setStates] = useState<SidecarState[]>([]);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const refresh = useCallback(async () => {
    try {
      const result = await sidecar.status();
      setStates(Array.isArray(result) ? result : [result]);
    } catch {
      // IPC not available (e.g. running outside Electron) — ignore.
    }
  }, []);

  useEffect(() => {
    refresh();
    intervalRef.current = setInterval(() => {
      if (!document.hidden) refresh();
    }, 5_000);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [refresh]);

  // Only show chips for non-stopped sidecars.
  const visible = states.filter((s) => s.status !== "stopped");
  if (visible.length === 0) return null;

  return (
    <div className="flex items-center gap-2">
      {visible.map((s) => (
        <SidecarChip key={s.id} state={s} onRefresh={refresh} />
      ))}
    </div>
  );
}

// ── Individual chip ──────────────────────────────────────────────────

function SidecarChip({ state, onRefresh }: { state: SidecarState; onRefresh: () => void }) {
  const def = SIDECAR_DEFS.find((d) => d.id === state.id);
  const label = def?.label ?? state.id;

  const handleOpen = () => {
    if (state.url) {
      window.open(state.url, "_blank", "noopener,noreferrer");
    }
  };

  const handleStop = async () => {
    await sidecar.stop(state.id);
    onRefresh();
  };

  return (
    <span className="inline-flex items-center gap-1.5 rounded-full border border-border bg-background px-2 py-[2px] text-[10px]">
      <StatusDot status={state.status} />
      <span className="text-foreground">{label}</span>
      {state.status === "running" && (
        <>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={handleOpen}
            className="ml-0.5 text-primary hover:text-foreground"
            title={`Open ${label}`}
            aria-label={`Open ${label} in browser`}
          >
            <ExternalLink size={10} />
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={handleStop}
            className="text-muted-foreground hover:text-destructive"
            title={`Stop ${label}`}
            aria-label={`Stop ${label}`}
          >
            <Square size={9} />
          </Button>
        </>
      )}
      {state.status === "error" && state.error && (
        <span className="text-destructive" title={state.error}>
          <AlertCircle size={10} />
        </span>
      )}
    </span>
  );
}

function StatusDot({ status }: { status: SidecarState["status"] }) {
  switch (status) {
    case "running":
      return <Circle size={6} className="fill-success text-success" />;
    case "starting":
      return <Loader2 size={8} className="animate-spin text-info" />;
    case "error":
      return <Circle size={6} className="fill-destructive text-destructive" />;
    default:
      return (
        <Circle size={6} className="fill-muted-foreground/60 text-muted-foreground/60" />
      );
  }
}

// ── SidecarLauncher (Dashboard / dedicated panel) ────────────────────

interface LauncherProps {
  /** Working directory for spawned sidecars (typically the repo root). */
  cwd: string;
}

/**
 * Card-based launcher for all supported sidecars. Shows each sidecar
 * with its icon, description, and start/stop/open controls.
 */
export function SidecarLauncher({ cwd }: LauncherProps) {
  const [states, setStates] = useState<SidecarState[]>([]);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const refresh = useCallback(async () => {
    try {
      const result = await sidecar.status();
      setStates(Array.isArray(result) ? result : [result]);
    } catch {
      // IPC not available — ignore.
    }
  }, []);

  useEffect(() => {
    refresh();
    intervalRef.current = setInterval(() => {
      if (!document.hidden) refresh();
    }, 5_000);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [refresh]);

  const stateFor = (id: SidecarId): SidecarState | undefined =>
    states.find((s) => s.id === id);

  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
      {SIDECAR_DEFS.map((def) => (
        <SidecarCard
          key={def.id}
          def={def}
          state={stateFor(def.id)}
          cwd={cwd}
          onRefresh={refresh}
        />
      ))}
    </div>
  );
}

// ── Sidecar card ─────────────────────────────────────────────────────

function SidecarCard({
  def,
  state,
  cwd,
  onRefresh,
}: {
  def: SidecarDef;
  state: SidecarState | undefined;
  cwd: string;
  onRefresh: () => void;
}) {
  const [starting, setStarting] = useState(false);
  const status = state?.status ?? "stopped";

  const handleStart = async () => {
    setStarting(true);
    try {
      await sidecar.start({ id: def.id, cwd, port: def.defaultPort });
    } catch {
      // Error will surface via status polling.
    } finally {
      setStarting(false);
      onRefresh();
    }
  };

  const handleStop = async () => {
    await sidecar.stop(def.id);
    onRefresh();
  };

  const handleOpen = () => {
    if (state?.url) {
      window.open(state.url, "_blank", "noopener,noreferrer");
    }
  };

  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="flex items-start gap-3">
        <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-background">
          <SidecarIcon icon={def.icon} size={16} className="text-primary" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium text-foreground">
              {def.label}
            </span>
            <StatusBadge status={status} />
          </div>
          <p className="mt-0.5 text-[11px] leading-relaxed text-muted-foreground">
            {def.description}
          </p>
        </div>
      </div>

      {/* URL display when running */}
      {status === "running" && state?.url && (
        <div className="mt-3 truncate rounded bg-background px-2 py-1 font-mono text-[10px] text-muted-foreground">
          {state.url}
        </div>
      )}

      {/* Error hint */}
      {status === "error" && (
        <div className="mt-3 rounded border border-destructive/30 bg-destructive/5 px-2 py-1.5 text-[11px] text-destructive">
          {state?.error ?? "Failed to start."}
          <span className="mt-1 block text-[10px] text-muted-foreground">
            Install: <code className="font-mono">{def.installHint}</code>
          </span>
        </div>
      )}

      {/* Actions */}
      <div className="mt-3 flex items-center gap-2">
        {status === "stopped" || status === "error" ? (
          <Button
            variant="default"
            size="compact"
            type="button"
            onClick={handleStart}
            disabled={starting}
            className="inline-flex items-center gap-1.5 rounded-md bg-primary px-3 py-1 text-[11px] font-medium text-primary-foreground transition hover:bg-primary/80 disabled:opacity-50"
            aria-label={`Start ${def.label}`}
          >
            {starting ? (
              <Loader2 size={11} className="animate-spin" />
            ) : (
              <Play size={11} />
            )}
            Start
          </Button>
        ) : status === "starting" ? (
          <span className="inline-flex items-center gap-1.5 text-[11px] text-info">
            <Loader2 size={11} className="animate-spin" /> Starting…
          </span>
        ) : null}

        {status === "running" && (
          <>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={handleOpen}
              className="inline-flex items-center gap-1.5 rounded-md border border-border px-3 py-1 text-[11px] text-foreground transition hover:bg-accent"
              aria-label={`Open ${def.label} in browser`}
            >
              <ExternalLink size={11} /> Open
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={handleStop}
              className="inline-flex items-center gap-1.5 rounded-md border border-destructive/40 px-3 py-1 text-[11px] text-destructive transition hover:bg-destructive/10"
              aria-label={`Stop ${def.label}`}
            >
              <Square size={11} /> Stop
            </Button>
          </>
        )}
      </div>
    </div>
  );
}

function StatusBadge({ status }: { status: SidecarState["status"] }) {
  switch (status) {
    case "running":
      return (
        <span className="inline-flex items-center gap-1 rounded-full bg-success/10 px-1.5 py-[1px] text-[9px] text-success">
          <Circle size={5} className="fill-current" /> Running
        </span>
      );
    case "starting":
      return (
        <span className="inline-flex items-center gap-1 rounded-full bg-info/10 px-1.5 py-[1px] text-[9px] text-info">
          Starting
        </span>
      );
    case "error":
      return (
        <span className="inline-flex items-center gap-1 rounded-full bg-destructive/10 px-1.5 py-[1px] text-[9px] text-destructive">
          Error
        </span>
      );
    default:
      return null;
  }
}
