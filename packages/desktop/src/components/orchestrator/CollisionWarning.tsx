import { AlertTriangle, CheckCircle2, Lightbulb, X } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

type CollisionConfidence = "high" | "medium" | "low";
type MitigationStrategy = "sequential" | "rebase" | "shared-workspace";

interface CollisionPrediction {
  file: string;
  taskIds: string[];
  confidence: CollisionConfidence;
  reason: string;
  mitigation: MitigationStrategy;
}

interface Props {
  predictions: CollisionPrediction[];
  onApplySuggestions: (predictions: CollisionPrediction[]) => void;
  onDismiss: () => void;
}

// ── Helpers ─────────────────────────────────────────────────────────

const CONFIDENCE_STYLES: Record<CollisionConfidence, { dot: string; label: string }> = {
  high: { dot: "bg-gx-error", label: "HIGH" },
  medium: { dot: "bg-gx-warn", label: "MED" },
  low: { dot: "bg-gx-muted", label: "LOW" },
};

const MITIGATION_LABELS: Record<MitigationStrategy, string> = {
  sequential: "Run sequentially",
  rebase: "Rebase after completion",
  "shared-workspace": "Use shared workspace",
};

// ── Component ───────────────────────────────────────────────────────

export function CollisionWarning({ predictions, onApplySuggestions, onDismiss }: Props) {
  if (predictions.length === 0) return null;

  const highCount = predictions.filter((p) => p.confidence === "high").length;
  const medCount = predictions.filter((p) => p.confidence === "medium").length;

  return (
    <div
      className="rounded-lg border border-gx-warn/40 bg-gx-warn/5 p-4"
      role="alert"
      aria-label={`${predictions.length} potential collision${predictions.length !== 1 ? "s" : ""} detected`}
    >
      {/* Header */}
      <div className="mb-3 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <AlertTriangle size={16} className="text-gx-warn" />
          <span className="text-sm font-medium text-gx-text">
            {predictions.length} potential conflict{predictions.length !== 1 ? "s" : ""} detected
          </span>
        </div>
        <button
          type="button"
          onClick={onDismiss}
          className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
          aria-label="Dismiss collision warnings"
        >
          <X size={14} />
        </button>
      </div>

      {/* Summary badges */}
      <div className="mb-3 flex items-center gap-2 text-[10px]">
        {highCount > 0 && (
          <span className="rounded-full bg-gx-error/15 px-2 py-0.5 text-gx-error">
            {highCount} high risk
          </span>
        )}
        {medCount > 0 && (
          <span className="rounded-full bg-gx-warn/15 px-2 py-0.5 text-gx-warn">
            {medCount} medium risk
          </span>
        )}
      </div>

      {/* Prediction list */}
      <ul className="space-y-2" role="list" aria-label="Collision predictions">
        {predictions.map((prediction, idx) => {
          const style = CONFIDENCE_STYLES[prediction.confidence];
          return (
            <li
              key={`${prediction.file}-${idx}`}
              className="rounded border border-gx-border/40 bg-gx-bg px-3 py-2"
            >
              <div className="flex items-start gap-2">
                {/* Confidence dot */}
                <span
                  className={`mt-1 h-2 w-2 shrink-0 rounded-full ${style.dot}`}
                  aria-label={`${style.label} confidence`}
                />

                <div className="min-w-0 flex-1">
                  {/* File path */}
                  <div className="flex items-center gap-2">
                    <span className="truncate font-mono text-xs text-gx-text">
                      {prediction.file}
                    </span>
                    <span className="shrink-0 text-[9px] font-medium uppercase text-gx-muted">
                      {style.label}
                    </span>
                  </div>

                  {/* Reason */}
                  <p className="mt-0.5 text-[11px] text-gx-muted line-clamp-2">
                    {prediction.reason}
                  </p>

                  {/* Mitigation suggestion */}
                  <div className="mt-1 flex items-center gap-1 text-[10px] text-gx-accent">
                    <Lightbulb size={10} />
                    {MITIGATION_LABELS[prediction.mitigation]}
                  </div>
                </div>
              </div>
            </li>
          );
        })}
      </ul>

      {/* Actions */}
      <div className="mt-3 flex items-center gap-2">
        <button
          type="button"
          onClick={() => onApplySuggestions(predictions)}
          className="flex items-center gap-1.5 rounded bg-gx-accent px-3 py-1.5 text-xs font-medium text-white transition hover:bg-gx-accent-hover"
          aria-label="Apply mitigation suggestions"
        >
          <CheckCircle2 size={12} />
          Apply suggestions
        </button>
        <button
          type="button"
          onClick={onDismiss}
          className="rounded border border-gx-border px-3 py-1.5 text-xs text-gx-text transition hover:bg-gx-surface-hover"
        >
          Proceed anyway
        </button>
      </div>
    </div>
  );
}
