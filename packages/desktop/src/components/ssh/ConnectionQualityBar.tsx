/**
 * Connection Quality Bar — color-coded icon for SSH status bar with
 * tooltip showing latency, bandwidth, and packet loss metrics.
 */

import { Wifi, WifiOff } from "lucide-react";
import clsx from "clsx";
import type { ConnectionQuality, QualityLevel } from "../../../electron/ssh-types";

// ── Types ────────────────────────────────────────────────────────

interface ConnectionQualityBarProps {
  quality: ConnectionQuality | null;
  profileLabel?: string;
}

// ── Helpers ──────────────────────────────────────────────────────

const QUALITY_COLORS: Record<QualityLevel, string> = {
  green: "text-green-500",
  yellow: "text-yellow-500",
  red: "text-red-500",
};

const QUALITY_BG_COLORS: Record<QualityLevel, string> = {
  green: "bg-green-500",
  yellow: "bg-yellow-500",
  red: "bg-red-500",
};

function formatLatency(ms: number): string {
  return `${Math.round(ms)} ms`;
}

function formatBandwidth(mbps: number): string {
  if (mbps < 1) return `${(mbps * 1000).toFixed(0)} Kbps`;
  return `${mbps.toFixed(1)} Mbps`;
}

function formatPacketLoss(percent: number): string {
  return `${percent.toFixed(1)}%`;
}

function getQualityLabel(level: QualityLevel): string {
  switch (level) {
    case "green":
      return "Good";
    case "yellow":
      return "Fair";
    case "red":
      return "Poor";
  }
}

// ── Component ────────────────────────────────────────────────────

export function ConnectionQualityBar({ quality, profileLabel }: ConnectionQualityBarProps) {
  if (!quality) {
    return (
      <div
        className="flex items-center gap-1 px-1.5 py-0.5"
        title="Connection quality unavailable"
        aria-label="Connection quality: unavailable"
      >
        <WifiOff className="w-3.5 h-3.5 text-gray-400 dark:text-gray-500" />
      </div>
    );
  }

  const tooltipText = [
    profileLabel ? `${profileLabel} — ${getQualityLabel(quality.level)}` : getQualityLabel(quality.level),
    `Latency: ${formatLatency(quality.latencyMs)}`,
    `Bandwidth: ${formatBandwidth(quality.bandwidthMbps)}`,
    `Packet Loss: ${formatPacketLoss(quality.packetLossPercent)}`,
  ].join("\n");

  return (
    <div
      className="relative flex items-center gap-1 px-1.5 py-0.5 cursor-default group"
      title={tooltipText}
      aria-label={`Connection quality: ${getQualityLabel(quality.level)}, latency ${formatLatency(quality.latencyMs)}, bandwidth ${formatBandwidth(quality.bandwidthMbps)}, packet loss ${formatPacketLoss(quality.packetLossPercent)}`}
    >
      {/* Color-coded icon */}
      <Wifi className={clsx("w-3.5 h-3.5", QUALITY_COLORS[quality.level])} />

      {/* Small dot indicator */}
      <span
        className={clsx("w-1.5 h-1.5 rounded-full", QUALITY_BG_COLORS[quality.level])}
        aria-hidden="true"
      />

      {/* Tooltip (shown on hover) */}
      <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-2 hidden group-hover:block z-50 pointer-events-none">
        <div className="bg-gray-900 dark:bg-gray-800 text-white rounded-md px-3 py-2 shadow-lg whitespace-nowrap text-xs">
          <div className="font-medium mb-1 flex items-center gap-1.5">
            <span
              className={clsx("w-2 h-2 rounded-full", QUALITY_BG_COLORS[quality.level])}
              aria-hidden="true"
            />
            {profileLabel ? `${profileLabel} — ${getQualityLabel(quality.level)}` : getQualityLabel(quality.level)}
          </div>
          <div className="space-y-0.5 text-gray-300">
            <p>Latency: {formatLatency(quality.latencyMs)}</p>
            <p>Bandwidth: {formatBandwidth(quality.bandwidthMbps)}</p>
            <p>Packet Loss: {formatPacketLoss(quality.packetLossPercent)}</p>
          </div>
          {/* Tooltip arrow */}
          <div className="absolute top-full left-1/2 -translate-x-1/2 w-0 h-0 border-l-4 border-r-4 border-t-4 border-transparent border-t-gray-900 dark:border-t-gray-800" />
        </div>
      </div>
    </div>
  );
}

// ── Exports for testing ──────────────────────────────────────────

export { formatLatency, formatBandwidth, formatPacketLoss, getQualityLabel, QUALITY_COLORS };
