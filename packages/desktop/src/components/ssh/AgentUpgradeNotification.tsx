/**
 * Agent Upgrade Notification — non-blocking notification for outdated agent
 * with upgrade offer, and disabled UI controls with tooltip showing minimum
 * version required.
 */

import { useState, useCallback } from "react";
import {
  ArrowUpCircle,
  X,
  Info,
  AlertCircle,
} from "lucide-react";
import type { AgentCapabilities } from "../../../electron/ssh-types";

// ── Types ────────────────────────────────────────────────────────

interface AgentUpgradeNotificationProps {
  /** Current agent version */
  currentVersion: string;
  /** Available (latest) agent version */
  availableVersion: string;
  /** Callback to initiate the upgrade */
  onUpgrade: () => void;
  /** Callback to dismiss the notification */
  onDismiss: () => void;
}

interface DisabledFeatureControlProps {
  /** The feature name to display */
  featureName: string;
  /** Minimum version required for this feature */
  minVersion: string;
  /** Current agent version */
  currentVersion: string;
  /** The child element to render as disabled */
  children: React.ReactNode;
}

// ── Capability Matrix ────────────────────────────────────────────

const FEATURES = [
  { label: "File watching", methods: ["watch_start", "watch_stop"] },
  { label: "Remote search", methods: ["git_grep"] },
  { label: "File materialization", methods: ["file_materialize"] },
  { label: "Remote commands", methods: ["exec_command"] },
];

// ── Upgrade Notification ─────────────────────────────────────────

export function AgentUpgradeNotification({
  currentVersion,
  availableVersion,
  onUpgrade,
  onDismiss,
}: AgentUpgradeNotificationProps) {
  const [dismissed, setDismissed] = useState(false);

  const handleDismiss = useCallback(() => {
    setDismissed(true);
    onDismiss();
  }, [onDismiss]);

  if (dismissed) return null;

  return (
    <div
      className="flex items-center gap-2 px-3 py-2 bg-info/10 bg-info/20 border border-info border-info rounded-md"
      role="alert"
      aria-label="Agent upgrade available"
    >
      <ArrowUpCircle className="w-4 h-4 text-info shrink-0" />
      <div className="flex-1 min-w-0">
        <p className="text-xs text-info text-info">
          Agent update available:{" "}
          <span className="font-mono">{currentVersion}</span>
          {" → "}
          <span className="font-mono font-medium">{availableVersion}</span>
        </p>
      </div>
      <div className="flex items-center gap-1 shrink-0">
        <button
          onClick={onUpgrade}
          className="px-2 py-0.5 text-xs font-medium text-info text-info bg-info/10 bg-info/50 hover:bg-info/10 hover:bg-info rounded"
        >
          Upgrade
        </button>
        <button
          onClick={handleDismiss}
          className="p-0.5 text-info hover:text-info hover:text-info rounded"
          aria-label="Dismiss upgrade notification"
        >
          <X className="w-3.5 h-3.5" />
        </button>
      </div>
    </div>
  );
}

// ── Disabled Feature Control ─────────────────────────────────────

export function DisabledFeatureControl({
  featureName,
  minVersion,
  currentVersion,
  children,
}: DisabledFeatureControlProps) {
  const tooltipText = `Requires agent v${minVersion} (current: v${currentVersion})`;

  return (
    <div
      className="relative group"
      title={tooltipText}
      aria-disabled="true"
      aria-label={`${featureName} — disabled, requires agent version ${minVersion}`}
    >
      {/* Disabled overlay */}
      <div className="opacity-50 pointer-events-none select-none">
        {children}
      </div>

      {/* Tooltip on hover */}
      <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-1.5 hidden group-hover:block z-50 pointer-events-none">
        <div className="bg-background bg-card text-foreground rounded px-2 py-1 shadow-lg whitespace-nowrap text-[10px] flex items-center gap-1">
          <Info className="w-3 h-3 text-muted-foreground" />
          <span>
            Requires agent v{minVersion}
          </span>
          {/* Arrow */}
          <div className="absolute top-full left-1/2 -translate-x-1/2 w-0 h-0 border-l-[4px] border-r-[4px] border-t-[4px] border-transparent border-t-gray-900 dark:border-t-gray-700" />
        </div>
      </div>
    </div>
  );
}

// ── Capability Summary ───────────────────────────────────────────

interface CapabilitySummaryProps {
  capabilities: AgentCapabilities | null;
  currentVersion: string;
}

export function CapabilitySummary({ capabilities, currentVersion }: CapabilitySummaryProps) {
  if (!capabilities) {
    return (
      <div className="flex items-center gap-2 px-3 py-2 text-xs text-muted-foreground text-muted-foreground">
        <AlertCircle className="w-4 h-4" />
        <span>Agent capabilities unknown</span>
      </div>
    );
  }

  const unsupported = FEATURES.filter(feature =>
    !feature.methods.every(method => capabilities.methods.includes(method)));

  if (unsupported.length === 0) {
    return null; // All features supported, nothing to show
  }

  return (
    <div className="px-3 py-2 space-y-1.5" role="region" aria-label="Unsupported features">
      <p className="text-[10px] text-muted-foreground text-muted-foreground font-medium">
        Unavailable features (agent v{currentVersion}):
      </p>
      <div className="flex flex-wrap gap-1">
        {unsupported.map(feature => (
          <span
            key={feature.label}
            className="inline-flex items-center gap-1 px-1.5 py-0.5 text-[10px] bg-card bg-card text-muted-foreground text-muted-foreground rounded"
            title={`Not supported by agent v${currentVersion}`}
          >
            {feature.label}
          </span>
        ))}
      </div>
    </div>
  );
}
