/**
 * DeployProgressIndicator — self-contained component that listens for
 * `ssh:deploy-progress` and `ssh:agent-unavailable` IPC events and renders
 * deployment progress inline (reusing DeployProgress) plus a dismissible
 * warning banner when the agent cannot be deployed.
 */

import { useState, useEffect, useCallback } from "react";
import { AlertTriangle, X } from "lucide-react";
import { DeployProgress } from "./DeployProgress";

// ── Types ────────────────────────────────────────────────────────

type DeployStage = "detecting" | "uploading" | "verifying" | "starting";

interface DeployProgressPayload {
  stage: string;
  percent: number;
  speed?: number;
  eta?: number;
}

interface AgentUnavailablePayload {
  profileId: string;
  reason: string;
  message: string;
}

export interface DeployProgressIndicatorProps {
  /** Filter events to a specific profile (optional — shows all if omitted) */
  profileId?: string;
}

// ── Helpers ──────────────────────────────────────────────────────

const VALID_STAGES: DeployStage[] = ["detecting", "uploading", "verifying", "starting"];

function isValidStage(stage: string): stage is DeployStage {
  return VALID_STAGES.includes(stage as DeployStage);
}

// ── Component ────────────────────────────────────────────────────

export function DeployProgressIndicator({ profileId }: DeployProgressIndicatorProps) {
  const [progress, setProgress] = useState<DeployProgressPayload | null>(null);
  const [unavailable, setUnavailable] = useState<AgentUnavailablePayload | null>(null);
  const [warningDismissed, setWarningDismissed] = useState(false);

  // Listen for deploy progress events
  useEffect(() => {
    const cleanup = (window as any).crab.ssh.onDeployProgress(
      (payload: DeployProgressPayload) => {
        setProgress(payload);
        // Clear any previous unavailable warning when a new deploy starts
        setUnavailable(null);
        setWarningDismissed(false);
      },
    );
    return () => { cleanup(); };
  }, []);

  // Listen for agent unavailable events
  useEffect(() => {
    const cleanup = (window as any).crab.ssh.onAgentUnavailable(
      (payload: AgentUnavailablePayload) => {
        // If filtering by profile, only show for matching profile
        if (profileId && payload.profileId !== profileId) return;
        setUnavailable(payload);
        setWarningDismissed(false);
        // Clear progress since deployment failed
        setProgress(null);
      },
    );
    return () => { cleanup(); };
  }, [profileId]);

  // Auto-hide progress when deployment completes (starting stage at 100%)
  useEffect(() => {
    if (progress?.stage === "starting" && progress.percent >= 100) {
      const timer = setTimeout(() => setProgress(null), 1500);
      return () => clearTimeout(timer);
    }
  }, [progress]);

  const handleDismissWarning = useCallback(() => {
    setWarningDismissed(true);
  }, []);

  // Determine the validated stage for the progress component
  const stage: DeployStage = progress && isValidStage(progress.stage)
    ? progress.stage
    : "detecting";

  return (
    <div className="space-y-2">
      {/* Deploy progress indicator */}
      {progress && (
        <DeployProgress
          stage={stage}
          percent={progress.percent}
          speed={progress.speed}
          eta={progress.eta}
          visible
        />
      )}

      {/* Agent unavailable warning banner */}
      {unavailable && !warningDismissed && (
        <div
          className="flex items-start gap-2 rounded-md border border-amber-200 dark:border-amber-800 bg-amber-50 dark:bg-amber-900/20 px-3 py-2"
          role="alert"
          aria-label="Agent deployment failed"
        >
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-500" aria-hidden="true" />
          <div className="min-w-0 flex-1">
            <p className="text-xs font-medium text-amber-700 dark:text-amber-300">
              Agent unavailable
            </p>
            <p className="mt-0.5 text-[11px] text-amber-600 dark:text-amber-400">
              {unavailable.message}
            </p>
          </div>
          <button
            onClick={handleDismissWarning}
            className="shrink-0 rounded p-0.5 text-amber-400 hover:text-amber-600 dark:hover:text-amber-300"
            aria-label="Dismiss warning"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </div>
      )}
    </div>
  );
}
