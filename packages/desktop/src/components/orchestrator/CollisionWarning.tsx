import { Button } from "@/components/ui/button";
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
  high: { dot: "bg-destructive", label: "HIGH" },
  medium: { dot: "bg-warning", label: "MED" },
  low: { dot: "bg-muted-foreground", label: "LOW" },
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
      className="rounded-lg border border-warning/40 bg-warning/5 p-4"
      role="alert"
      aria-label={`${predictions.length} potential collision${predictions.length !== 1 ? "s" : ""} detected`}
    >
      {/* Header */}
      <div className="mb-3 flex items-center justify-between">
        <div className="flex items-center gap-2">
          <AlertTriangle size={16} className="text-warning" />
          <span className="text-sm font-medium text-foreground">
            {predictions.length} potential conflict
            {predictions.length !== 1 ? "s" : ""} detected
          </span>
        </div>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onDismiss}
          className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
          aria-label="Dismiss collision warnings"
        >
          <X size={14} />
        </Button>
      </div>

      {/* Summary badges */}
      <div className="mb-3 flex items-center gap-2 text-[10px]">
        {highCount > 0 && (
          <span className="rounded-full bg-destructive/15 px-2 py-0.5 text-destructive">
            {highCount} high risk
          </span>
        )}
        {medCount > 0 && (
          <span className="rounded-full bg-warning/15 px-2 py-0.5 text-warning">
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
              className="rounded border border-border/40 bg-background px-3 py-2"
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
                    <span className="truncate font-mono text-xs text-foreground">
                      {prediction.file}
                    </span>
                    <span className="shrink-0 text-[9px] font-medium uppercase text-muted-foreground">
                      {style.label}
                    </span>
                  </div>

                  {/* Reason */}
                  <p className="mt-0.5 text-[11px] text-muted-foreground line-clamp-2">
                    {prediction.reason}
                  </p>

                  {/* Mitigation suggestion */}
                  <div className="mt-1 flex items-center gap-1 text-[10px] text-primary">
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
        <Button
          variant="default"
          size="compact"
          type="button"
          onClick={() => onApplySuggestions(predictions)}
          className="flex items-center gap-1.5 rounded bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground transition hover:bg-primary-hover"
          aria-label="Apply mitigation suggestions"
        >
          <CheckCircle2 size={12} />
          Apply suggestions
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onDismiss}
          className="rounded border border-border px-3 py-1.5 text-xs text-foreground transition hover:bg-accent"
        >
          Proceed anyway
        </Button>
      </div>
    </div>
  );
}
