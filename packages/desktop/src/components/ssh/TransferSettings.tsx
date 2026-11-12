import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
/**
 * TransferSettings — bandwidth limit, compression toggle, and adaptive
 * throttling configuration panel.
 *
 * Displays current limit and actual throughput in the panel header,
 * updating at intervals <= 1 second.
 */

import clsx from "clsx";
import { Activity, Gauge, Settings2, Zap } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { formatSpeed } from "./TransferProgressBar";

// ── Types ────────────────────────────────────────────────────────

export interface TransferSettingsProps {
  /** Current bandwidth limit in KB/s (0 = unlimited) */
  bandwidthLimit: number;
  /** Current actual throughput in bytes/sec */
  actualThroughput: number;
  /** Whether compression is enabled */
  compressionEnabled: boolean;
  /** Whether adaptive throttling is enabled */
  adaptiveThrottling: boolean;
  /** SSH profile ID (used for IPC context) */
  profileId?: string;
}

// ── Constants ────────────────────────────────────────────────────

const MIN_BANDWIDTH = 1; // KB/s
const MAX_BANDWIDTH = 102_400; // KB/s
const THROUGHPUT_POLL_INTERVAL = 1000; // 1 second

// ── Component ────────────────────────────────────────────────────

export function TransferSettings({
  bandwidthLimit: initialLimit,
  actualThroughput: initialThroughput,
  compressionEnabled: initialCompression,
  adaptiveThrottling: initialAdaptive,
}: TransferSettingsProps) {
  const [bandwidthLimit, setBandwidthLimit] = useState(initialLimit);
  const [inputValue, setInputValue] = useState(initialLimit === 0 ? "" : String(initialLimit));
  const [actualThroughput, setActualThroughput] = useState(initialThroughput);
  const [compressionEnabled, setCompressionEnabled] = useState(initialCompression);
  const [adaptiveThrottling, setAdaptiveThrottling] = useState(initialAdaptive);
  const [inputError, setInputError] = useState<string | null>(null);
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  // Sync props when they change externally
  useEffect(() => {
    setBandwidthLimit(initialLimit);
    setInputValue(initialLimit === 0 ? "" : String(initialLimit));
  }, [initialLimit]);

  useEffect(() => setActualThroughput(initialThroughput), [initialThroughput]);
  useEffect(() => setCompressionEnabled(initialCompression), [initialCompression]);
  useEffect(() => setAdaptiveThrottling(initialAdaptive), [initialAdaptive]);

  // Poll actual throughput every second
  useEffect(() => {
    pollRef.current = setInterval(async () => {
      try {
        const data = await window.crab.ssh.transferGetBandwidth();
        setActualThroughput(data.actual * 1024); // Convert KB/s to bytes/s
        setBandwidthLimit(data.limit);
      } catch {
        // Silently ignore poll failures
      }
    }, THROUGHPUT_POLL_INTERVAL);

    return () => {
      if (pollRef.current) clearInterval(pollRef.current);
    };
  }, []);

  // Validate and apply bandwidth limit
  const handleBandwidthChange = useCallback((value: string) => {
    setInputValue(value);
    setInputError(null);

    if (value.trim() === "" || value === "0") {
      // Unlimited
      setInputError(null);
      return;
    }

    const num = Number(value);
    if (!Number.isFinite(num) || num < 0) {
      setInputError("Enter a valid number");
    } else if (num > 0 && num < MIN_BANDWIDTH) {
      setInputError(`Minimum is ${MIN_BANDWIDTH} KB/s`);
    } else if (num > MAX_BANDWIDTH) {
      setInputError(`Maximum is ${MAX_BANDWIDTH.toLocaleString()} KB/s`);
    }
  }, []);

  const applyBandwidthLimit = useCallback(async () => {
    const trimmed = inputValue.trim();
    const num = trimmed === "" ? 0 : Number(trimmed);

    if (num !== 0 && (num < MIN_BANDWIDTH || num > MAX_BANDWIDTH)) return;

    try {
      await window.crab.ssh.transferSetBandwidth(num);
      setBandwidthLimit(num);
      setInputError(null);
    } catch {
      setInputError("Failed to apply limit");
    }
  }, [inputValue]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Enter") applyBandwidthLimit();
    },
    [applyBandwidthLimit],
  );

  const toggleCompression = useCallback(async () => {
    const next = !compressionEnabled;
    try {
      await window.crab.ssh.transferSetCompression(next);
      setCompressionEnabled(next);
    } catch {
      // Revert on failure
    }
  }, [compressionEnabled]);

  const toggleAdaptive = useCallback(async () => {
    const next = !adaptiveThrottling;
    try {
      await window.crab.ssh.transferSetAdaptive(next);
      setAdaptiveThrottling(next);
    } catch {
      // Revert on failure
    }
  }, [adaptiveThrottling]);

  return (
    <div className="flex flex-col gap-4 p-4" role="region" aria-label="Transfer settings">
      {/* Bandwidth limit input */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <Settings2 className="w-4 h-4 text-muted-foreground text-muted-foreground" />
          <h3 className="text-sm font-semibold text-foreground text-foreground">
            Transfer Settings
          </h3>
        </div>
        <div className="flex items-center gap-3 text-xs text-muted-foreground text-muted-foreground">
          <span>
            Limit:{" "}
            {bandwidthLimit === 0 ? "Unlimited" : `${bandwidthLimit.toLocaleString()} KB/s`}
          </span>
          <span className="text-muted-foreground text-muted-foreground">·</span>
          <span>Actual: {formatSpeed(actualThroughput)}</span>
        </div>
      </div>

      {/* Bandwidth limit input */}
      <div className="space-y-1.5">
        <label
          htmlFor="bandwidth-limit"
          className="flex items-center gap-1.5 text-xs font-medium text-muted-foreground text-muted-foreground"
        >
          <Gauge size={12} />
          Bandwidth Limit (KB/s)
        </label>
        <div className="flex items-center gap-2">
          <Input
            id="bandwidth-limit"
            type="number"
            min={0}
            max={MAX_BANDWIDTH}
            value={inputValue}
            onChange={(e) => handleBandwidthChange(e.target.value)}
            onBlur={applyBandwidthLimit}
            onKeyDown={handleKeyDown}
            placeholder="0 = unlimited"
            className={clsx(
              "flex-1 px-2.5 py-1.5 rounded-md text-sm",
              "bg-background bg-background",
              "border transition-colors",
              inputError
                ? "border-destructive border-destructive focus:ring-destructive"
                : "border-border border-border focus:ring-info",
              "focus:outline-none focus:ring-1",
            )}
            aria-describedby={inputError ? "bandwidth-error" : undefined}
            aria-invalid={!!inputError}
          />
          <span className="text-xs text-muted-foreground text-muted-foreground whitespace-nowrap">
            KB/s
          </span>
        </div>
        {inputError && (
          <p id="bandwidth-error" className="text-xs text-destructive text-destructive">
            {inputError}
          </p>
        )}
        <p className="text-xs text-muted-foreground text-muted-foreground">
          Range: {MIN_BANDWIDTH}–{MAX_BANDWIDTH.toLocaleString()} KB/s. Enter 0
          or leave empty for unlimited.
        </p>
      </div>

      {/* Adaptive throttling toggle */}
      <div className="flex items-center justify-between py-2 border-t border-border border-border">
        <div className="flex items-center gap-2">
          <Zap size={14} className="text-muted-foreground text-muted-foreground" />
          <div>
            <p className="text-sm font-medium text-foreground text-foreground">
              Compression
            </p>
            <p className="text-xs text-muted-foreground text-muted-foreground">
              Compress files with zstd before transfer
            </p>
          </div>
        </div>
        <Switch
          checked={compressionEnabled}
          onCheckedChange={toggleCompression}
        />
      </div>

      {/* Adaptive throttling toggle */}
      <div className="flex items-center justify-between py-2 border-t border-border border-border">
        <div className="flex items-center gap-2">
          <Activity size={14} className="text-muted-foreground text-muted-foreground" />
          <div>
            <p className="text-sm font-medium text-foreground text-foreground">
              Adaptive Throttling
            </p>
            <p className="text-xs text-muted-foreground text-muted-foreground">
              Reduce bandwidth when session latency is high
            </p>
          </div>
        </div>
        <Switch checked={adaptiveThrottling} onCheckedChange={toggleAdaptive} />
      </div>
    </div>
  );
}
