import clsx from "clsx";
import { CheckCircle2, Cpu, HardDrive, Loader2, Play, Upload } from "lucide-react";

export interface DeployProgressProps {
  stage: "detecting" | "uploading" | "verifying" | "starting";
  percent: number;
  speed?: number; // bytes/sec
  eta?: number; // seconds remaining
  visible: boolean;
}

const STAGES = ["detecting", "uploading", "verifying", "starting"] as const;

const STAGE_META: Record<
  (typeof STAGES)[number],
  { label: string; icon: typeof Cpu }
> = {
  detecting: { label: "Detecting architecture", icon: Cpu },
  uploading: { label: "Uploading agent", icon: Upload },
  verifying: { label: "Verifying binary", icon: HardDrive },
  starting: { label: "Starting agent", icon: Play },
};

/**
 * Displays deployment progress for the remote agent binary.
 * Shows a stage indicator with icons, a progress bar during upload,
 * and transfer speed / ETA information.
 */
export function DeployProgress({
  stage,
  percent,
  speed,
  eta,
  visible,
}: DeployProgressProps) {
  if (!visible) return null;

  const currentIndex = STAGES.indexOf(stage);

  return (
    <div
      className="rounded-lg border border-gx-border bg-gx-surface p-4"
      role="status"
      aria-label={`Deploying agent: ${STAGE_META[stage].label}`}
      aria-live="polite"
    >
      {/* Stage indicators */}
      <div className="flex items-center gap-1 mb-3">
        {STAGES.map((s, i) => {
          const meta = STAGE_META[s];
          const Icon = meta.icon;
          const isActive = i === currentIndex;
          const isComplete = i < currentIndex;

          return (
            <div key={s} className="flex items-center gap-1">
              {i > 0 && (
                <div
                  className={clsx(
                    "h-px w-4",
                    isComplete ? "bg-gx-accent" : "bg-gx-border",
                  )}
                />
              )}
              <div
                className={clsx(
                  "flex items-center gap-1.5 rounded-full px-2 py-1 text-[11px] font-medium transition-colors",
                  isActive && "bg-gx-accent/15 text-gx-accent",
                  isComplete && "text-gx-accent",
                  !isActive && !isComplete && "text-gx-muted",
                )}
              >
                {isComplete ? (
                  <CheckCircle2 size={14} aria-hidden="true" />
                ) : isActive ? (
                  <Loader2
                    size={14}
                    className="animate-spin"
                    aria-hidden="true"
                  />
                ) : (
                  <Icon size={14} aria-hidden="true" />
                )}
                <span className="hidden sm:inline">{meta.label}</span>
              </div>
            </div>
          );
        })}
      </div>

      {/* Progress bar (shown during upload) */}
      {stage === "uploading" && (
        <div className="space-y-2">
          <div
            className="h-2 w-full overflow-hidden rounded-full bg-gx-bg-sub"
            role="progressbar"
            aria-valuenow={percent}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-label="Upload progress"
          >
            <div
              className="h-full rounded-full bg-gx-accent transition-all duration-300 ease-out"
              style={{ width: `${Math.min(percent, 100)}%` }}
            />
          </div>

          {/* Speed and ETA */}
          <div className="flex items-center justify-between text-[11px] text-gx-muted">
            <span>{percent}%</span>
            <div className="flex items-center gap-3">
              {speed != null && speed > 0 && (
                <span>{formatSpeed(speed)}</span>
              )}
              {eta != null && eta > 0 && (
                <span>{formatEta(eta)} remaining</span>
              )}
            </div>
          </div>
        </div>
      )}

      {/* Non-upload stages show a simple status line */}
      {stage !== "uploading" && (
        <p className="text-[12px] text-gx-muted">
          {STAGE_META[stage].label}…
        </p>
      )}
    </div>
  );
}

function formatSpeed(bytesPerSec: number): string {
  if (bytesPerSec >= 1024 * 1024) {
    return `${(bytesPerSec / (1024 * 1024)).toFixed(1)} MB/s`;
  }
  if (bytesPerSec >= 1024) {
    return `${(bytesPerSec / 1024).toFixed(0)} KB/s`;
  }
  return `${bytesPerSec.toFixed(0)} B/s`;
}

function formatEta(seconds: number): string {
  if (seconds < 60) {
    return `${Math.ceil(seconds)}s`;
  }
  const mins = Math.floor(seconds / 60);
  const secs = Math.ceil(seconds % 60);
  return `${mins}m ${secs}s`;
}
