import clsx from "clsx";
import { CheckCircle2, Clock, Loader2, XCircle } from "lucide-react";
import type { CiCheck, CiCheckConclusion, CiCheckStatus } from "../ipc";

// ── Aggregate status derivation ─────────────────────────────────────

export type AggregateCheckStatus = "success" | "failure" | "pending" | "running";

/**
 * Derive a single aggregate status from a list of CI checks.
 * Priority: failure > running > pending > success.
 */
export function aggregateCheckStatus(checks: CiCheck[]): AggregateCheckStatus {
  if (!checks.length) return "success";

  let hasRunning = false;
  let hasPending = false;

  for (const check of checks) {
    if (check.status === "completed") {
      if (check.conclusion === "failure" || check.conclusion === "timed_out") {
        return "failure";
      }
    } else if (check.status === "in_progress") {
      hasRunning = true;
    } else if (check.status === "queued") {
      hasPending = true;
    }
  }

  if (hasRunning) return "running";
  if (hasPending) return "pending";
  return "success";
}

// ── Chip styles ─────────────────────────────────────────────────────

const STATUS_CONFIG: Record<
  AggregateCheckStatus,
  { icon: typeof CheckCircle2; color: string; label: string }
> = {
  success: { icon: CheckCircle2, color: "text-success", label: "Checks passed" },
  failure: { icon: XCircle, color: "text-destructive", label: "Checks failed" },
  pending: { icon: Clock, color: "text-warning", label: "Checks pending" },
  running: { icon: Loader2, color: "text-warning", label: "Checks running" },
};

// ── Compact chip (for Timeline commit rows) ─────────────────────────

interface CiStatusChipProps {
  checks: CiCheck[];
  /** When true, clicking a failing check opens its URL. */
  clickable?: boolean;
  /** Size variant. */
  size?: "sm" | "md";
}

/**
 * Compact CI status chip showing aggregate status.
 * Clicking opens the first failing check URL (or first check URL)
 * in the system browser.
 */
export function CiStatusChip({ checks, clickable = true, size = "sm" }: CiStatusChipProps) {
  if (!checks.length) return null;

  const status = aggregateCheckStatus(checks);
  const config = STATUS_CONFIG[status];
  const Icon = config.icon;
  const isSpinner = status === "running";

  const handleClick = (e: React.MouseEvent) => {
    if (!clickable) return;
    e.stopPropagation();
    // Find the first failing check with a URL, or fall back to the first check with a URL.
    const failingWithUrl = checks.find(
      (c) =>
        c.status === "completed" &&
        (c.conclusion === "failure" || c.conclusion === "timed_out") &&
        c.url,
    );
    const target = failingWithUrl ?? checks.find((c) => c.url);
    if (target?.url) {
      window.open(target.url, "_blank", "noopener,noreferrer");
    }
  };

  const iconSize = size === "sm" ? 9 : 11;

  return (
    <span
      className={clsx(
        "inline-flex shrink-0 items-center gap-0.5 rounded-full border border-current/20 px-1 py-[1px] font-medium opacity-80 hover:opacity-100",
        config.color,
        size === "sm" ? "text-[9px]" : "text-[10px]",
        clickable && "cursor-pointer",
      )}
      title={config.label}
      onClick={handleClick}
      role={clickable ? "button" : undefined}
      aria-label={config.label}
    >
      <Icon size={iconSize} className={isSpinner ? "animate-spin" : undefined} />
      {size === "md" && <span className="ml-0.5">{checks.length}</span>}
    </span>
  );
}

// ── Expanded check list (for PR detail view) ─────────────────────────

interface CiChecksListProps {
  checks: CiCheck[];
}

function conclusionIcon(status: CiCheckStatus, conclusion: CiCheckConclusion) {
  if (status === "queued") return <Clock size={12} className="text-warning" />;
  if (status === "in_progress") return <Loader2 size={12} className="animate-spin text-warning" />;
  // completed
  if (conclusion === "success" || conclusion === "neutral" || conclusion === "skipped") {
    return <CheckCircle2 size={12} className="text-success" />;
  }
  return <XCircle size={12} className="text-destructive" />;
}

/**
 * Expanded list of individual CI checks with names and statuses.
 * Each check with a URL is clickable.
 */
export function CiChecksList({ checks }: CiChecksListProps) {
  if (!checks.length) return null;

  return (
    <div className="space-y-1">
      {checks.map((check) => (
        <div
          key={check.name}
          className={clsx(
            "flex items-center gap-2 rounded px-2 py-1 text-[11px]",
            check.url && "cursor-pointer hover:bg-accent",
          )}
          onClick={() => {
            if (check.url) {
              window.open(check.url, "_blank", "noopener,noreferrer");
            }
          }}
          role={check.url ? "button" : undefined}
          aria-label={`${check.name}: ${check.status}${check.conclusion ? ` (${check.conclusion})` : ""}`}
        >
          {conclusionIcon(check.status, check.conclusion)}
          <span className="min-w-0 flex-1 truncate text-foreground">{check.name}</span>
          <span className="shrink-0 text-[10px] text-muted-foreground">
            {check.status === "completed" ? (check.conclusion ?? "done") : check.status}
          </span>
        </div>
      ))}
    </div>
  );
}
