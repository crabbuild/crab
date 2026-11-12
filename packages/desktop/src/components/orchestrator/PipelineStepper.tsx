import { Button } from "@/components/ui/button";
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
        <div className="flex h-6 w-6 items-center justify-center rounded-full bg-success/20">
          <Check size={12} className="text-success" />
        </div>
      );
    case "active":
      return (
        <div className="flex h-6 w-6 items-center justify-center rounded-full bg-primary/20">
          <Loader2 size={12} className="animate-spin text-primary" />
        </div>
      );
    case "failed":
      return (
        <div className="flex h-6 w-6 items-center justify-center rounded-full bg-destructive/20">
          <XCircle size={12} className="text-destructive" />
        </div>
      );
    case "pending":
    default:
      return (
        <div className="flex h-6 w-6 items-center justify-center rounded-full bg-background border border-border/60">
          <Circle size={10} className="text-muted-foreground/50" />
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
      className="rounded border border-border bg-background px-4 py-3"
      role="navigation"
      aria-label={`Pipeline: ${pipeline.name}`}
    >
      {/* Pipeline name + status */}
      <div className="mb-2 flex items-center justify-between">
        <span className="text-[11px] font-medium text-muted-foreground">
          Pipeline: {pipeline.name}
        </span>
        <span
          className={`text-[10px] font-medium uppercase ${
            status === "completed"
              ? "text-success"
              : status === "failed" || status === "aborted"
                ? "text-destructive"
                : "text-primary"
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
                      ? "font-medium text-primary"
                      : stepStatus === "done"
                        ? "text-success"
                        : stepStatus === "failed"
                          ? "text-destructive"
                          : "text-muted-foreground/60"
                  }`}
                >
                  {getStageLabel(stage.type)}
                </span>
              </div>

              {/* Connector line */}
              {!isLast && (
                <div
                  className={`mx-1 h-px w-6 ${
                    stepStatus === "done" ? "bg-success/50" : "bg-border"
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
        <div className="mt-3 flex items-center gap-2 border-t border-border/40 pt-2">
          {needsApproval && (
            <>
              <span className="text-[10px] text-muted-foreground">
                Waiting for approval: {getStageLabel(currentStage.type)}
              </span>
              <Button
                variant="default"
                size="compact"
                type="button"
                onClick={() => onApprove?.(currentStageIndex)}
                className="rounded bg-primary px-2.5 py-1 text-[10px] font-medium text-primary-foreground transition hover:bg-primary-hover"
                aria-label={`Approve ${getStageLabel(currentStage.type)} stage`}
              >
                Approve
              </Button>
            </>
          )}
          {onAbort && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              onClick={onAbort}
              className="ml-auto rounded border border-border px-2.5 py-1 text-[10px] text-muted-foreground transition hover:border-destructive/60 hover:text-destructive"
              aria-label="Abort pipeline"
            >
              Abort
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
