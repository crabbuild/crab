import { Check, Circle, Loader2, XCircle } from "lucide-react";

// ── Types ───────────────────────────────────────────────────────────

type BuiltinStageType =
  | "implement"
  | "test"
  | "review"
  | "create-pr"
  | "ci-pass"
  | "merge"
  | "archive";

type StageType = BuiltinStageType | { custom: string };

type PipelineStatus = "running" | "paused" | "completed" | "failed" | "aborted";

interface StageResult {
  stageIndex: number;
  type: StageType;
  success: boolean;
  error?: string;
  startedAt: number;
  completedAt?: number;
}

interface PipelineStage {
  type: StageType;
  gate?: "approval" | "condition";
  condition?: string;
  timeout?: number;
}

interface Pipeline {
  name: string;
  stages: PipelineStage[];
}

interface PipelineState {
  id: string;
  workspaceId: string;
  pipeline: Pipeline;
  currentStageIndex: number;
  stageResults: StageResult[];
  status: PipelineStatus;
  createdAt: number;
  updatedAt: number;
}

interface Props {
  state: PipelineState;
  onApprove?: (stageIndex: number) => void;
  onAbort?: () => void;
}

// ── Helpers ─────────────────────────────────────────────────────────

function getStageLabel(type: StageType): string {
  if (typeof type === "string") {
    const labels: Record<BuiltinStageType, string> = {
      implement: "Implement",
      test: "Test",
      review: "Review",
      "create-pr": "PR",
      "ci-pass": "CI",
      merge: "Merge",
      archive: "Archive",
    };
    return labels[type] ?? type;
  }
  return type.custom;
}

type StepStatus = "done" | "active" | "pending" | "failed";

function getStepStatus(
  stageIndex: number,
  currentIndex: number,
  stageResults: StageResult[],
  pipelineStatus: PipelineStatus,
): StepStatus {
  const result = stageResults.find((r) => r.stageIndex === stageIndex);

  if (result) {
    return result.success ? "done" : "failed";
  }

  if (stageIndex === currentIndex) {
    if (pipelineStatus === "failed") return "failed";
    return "active";
  }

  return "pending";
}

function StepIcon({ status }: { status: StepStatus }) {
  switch (status) {
    case "done":
      return (
        <div className="flex h-6 w-6 items-center justify-center rounded-full bg-gx-success/20">
          <Check size={12} className="text-gx-success" />
        </div>
      );
    case "active":
      return (
        <div className="flex h-6 w-6 items-center justify-center rounded-full bg-gx-accent/20">
          <Loader2 size={12} className="animate-spin text-gx-accent" />
        </div>
      );
    case "failed":
      return (
        <div className="flex h-6 w-6 items-center justify-center rounded-full bg-gx-error/20">
          <XCircle size={12} className="text-gx-error" />
        </div>
      );
    case "pending":
    default:
      return (
        <div className="flex h-6 w-6 items-center justify-center rounded-full bg-gx-bg-sub border border-gx-border/60">
          <Circle size={10} className="text-gx-muted/50" />
        </div>
      );
  }
}

// ── Component ───────────────────────────────────────────────────────

export function PipelineStepper({ state, onApprove, onAbort }: Props) {
  const { pipeline, currentStageIndex, stageResults, status } = state;
  const stages = pipeline.stages;
  const isPaused = status === "paused";
  const currentStage = stages[currentStageIndex];
  const needsApproval = isPaused && currentStage?.gate === "approval";

  return (
    <div
      className="rounded border border-gx-border bg-gx-bg-sub px-4 py-3"
      role="navigation"
      aria-label={`Pipeline: ${pipeline.name}`}
    >
      {/* Pipeline name + status */}
      <div className="mb-2 flex items-center justify-between">
        <span className="text-[11px] font-medium text-gx-muted">
          Pipeline: {pipeline.name}
        </span>
        <span
          className={`text-[10px] font-medium uppercase ${
            status === "completed"
              ? "text-gx-success"
              : status === "failed" || status === "aborted"
                ? "text-gx-error"
                : "text-gx-accent"
          }`}
        >
          {status}
        </span>
      </div>

      {/* Stepper */}
      <div
        className="flex items-center gap-1 overflow-x-auto"
        role="list"
        aria-label="Pipeline stages"
      >
        {stages.map((stage, idx) => {
          const stepStatus = getStepStatus(idx, currentStageIndex, stageResults, status);
          const isLast = idx === stages.length - 1;

          return (
            <div
              key={idx}
              className="flex items-center"
              role="listitem"
              aria-label={`${getStageLabel(stage.type)} — ${stepStatus}`}
              aria-current={stepStatus === "active" ? "step" : undefined}
            >
              {/* Step */}
              <div className="flex flex-col items-center gap-0.5">
                <StepIcon status={stepStatus} />
                <span
                  className={`text-[9px] ${
                    stepStatus === "active"
                      ? "font-medium text-gx-accent"
                      : stepStatus === "done"
                        ? "text-gx-success"
                        : stepStatus === "failed"
                          ? "text-gx-error"
                          : "text-gx-muted/60"
                  }`}
                >
                  {getStageLabel(stage.type)}
                </span>
              </div>

              {/* Connector line */}
              {!isLast && (
                <div
                  className={`mx-1 h-px w-6 ${
                    stepStatus === "done" ? "bg-gx-success/50" : "bg-gx-border"
                  }`}
                  aria-hidden="true"
                />
              )}
            </div>
          );
        })}
      </div>

      {/* Approval / abort actions */}
      {(needsApproval || status === "running") && (
        <div className="mt-3 flex items-center gap-2 border-t border-gx-border/40 pt-2">
          {needsApproval && (
            <>
              <span className="text-[10px] text-gx-muted">
                Waiting for approval: {getStageLabel(currentStage.type)}
              </span>
              <button
                type="button"
                onClick={() => onApprove?.(currentStageIndex)}
                className="rounded bg-gx-accent px-2.5 py-1 text-[10px] font-medium text-white transition hover:bg-gx-accent-hover"
                aria-label={`Approve ${getStageLabel(currentStage.type)} stage`}
              >
                Approve
              </button>
            </>
          )}
          {onAbort && (
            <button
              type="button"
              onClick={onAbort}
              className="ml-auto rounded border border-gx-border px-2.5 py-1 text-[10px] text-gx-muted transition hover:border-gx-error/60 hover:text-gx-error"
              aria-label="Abort pipeline"
            >
              Abort
            </button>
          )}
        </div>
      )}
    </div>
  );
}
