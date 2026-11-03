/**
 * TransferScheduleSection — displays scheduled transfers and allows
 * creating/cancelling scheduled transfers.
 *
 * Shows file name, destination, planned start time, delta savings,
 * and compressed sizes. Validates that scheduled times are in the future.
 */

import { useState, useCallback, useEffect } from "react";
import {
  Clock,
  Calendar,
  X,
  Plus,
  AlertCircle,
  TrendingDown,
  Archive,
} from "lucide-react";
import clsx from "clsx";
import type { ScheduledTransfer, TransferDirection } from "../../../electron/ssh-types";
import { formatBytes } from "./TransferProgressBar";

// ── Types ────────────────────────────────────────────────────────

export interface TransferScheduleSectionProps {
  /** SSH profile ID */
  profileId: string;
  /** Active transfers to show delta/compression info for */
  transfers?: Map<string, { deltaSavingsPercent?: number; totalBytes: number; compressedBytes?: number }>;
}

interface ScheduleFormData {
  localPath: string;
  remotePath: string;
  direction: TransferDirection;
  scheduledAt: string; // datetime-local input value
}

// ── Component ────────────────────────────────────────────────────

export function TransferScheduleSection({ profileId, transfers }: TransferScheduleSectionProps) {
  const [scheduled, setScheduled] = useState<ScheduledTransfer[]>([]);
  const [showForm, setShowForm] = useState(false);
  const [formData, setFormData] = useState<ScheduleFormData>({
    localPath: "",
    remotePath: "",
    direction: "upload",
    scheduledAt: "",
  });
  const [formError, setFormError] = useState<string | null>(null);
  const [isSubmitting, setIsSubmitting] = useState(false);

  // Fetch scheduled transfers
  const fetchScheduled = useCallback(async () => {
    try {
      const result = await window.crab.ssh.transferListScheduled(profileId);
      setScheduled(result);
    } catch {
      setScheduled([]);
    }
  }, [profileId]);

  useEffect(() => { fetchScheduled(); }, [fetchScheduled]);

  // Cancel a scheduled transfer
  const handleCancel = useCallback(async (transferId: string) => {
    try {
      await window.crab.ssh.transferCancelSchedule(transferId);
      setScheduled((prev) => prev.filter((s) => s.id !== transferId));
    } catch {
      // Silently fail — user can retry
    }
  }, []);

  // Create a new scheduled transfer
  const handleSubmit = useCallback(async () => {
    setFormError(null);

    // Validate required fields
    if (!formData.localPath.trim() || !formData.remotePath.trim()) {
      setFormError("Local path and remote path are required");
      return;
    }

    if (!formData.scheduledAt) {
      setFormError("Scheduled time is required");
      return;
    }

    // Validate future time
    const scheduledTime = new Date(formData.scheduledAt).getTime();
    if (scheduledTime <= Date.now()) {
      setFormError("Scheduled time must be in the future");
      return;
    }

    setIsSubmitting(true);
    try {
      await window.crab.ssh.transferSchedule({
        profileId,
        direction: formData.direction,
        localPath: formData.localPath.trim(),
        remotePath: formData.remotePath.trim(),
        scheduledAt: scheduledTime,
      });

      // Reset form and refresh list
      setFormData({ localPath: "", remotePath: "", direction: "upload", scheduledAt: "" });
      setShowForm(false);
      await fetchScheduled();
    } catch (err: any) {
      setFormError(err?.message || "Failed to schedule transfer");
    } finally {
      setIsSubmitting(false);
    }
  }, [formData, profileId, fetchScheduled]);

  // Get minimum datetime for the input (now + 1 minute)
  const minDateTime = new Date(Date.now() + 60_000).toISOString().slice(0, 16);

  return (
    <div className="flex flex-col gap-3" role="region" aria-label="Scheduled transfers">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-gray-100 dark:border-gray-700">
        <div className="flex items-center gap-2">
          <Calendar className="w-4 h-4 text-purple-500 dark:text-purple-400" />
          <h3 className="text-xs font-semibold text-gray-600 dark:text-gray-300">
            Scheduled Transfers
          </h3>
          {scheduled.length > 0 && (
            <span className="text-xs text-gray-400 dark:text-gray-500">
              ({scheduled.length})
            </span>
          )}
        </div>
        <button
          type="button"
          onClick={() => setShowForm(!showForm)}
          className="flex items-center gap-1 px-2 py-1 text-xs font-medium text-purple-600 dark:text-purple-400 hover:bg-purple-50 dark:hover:bg-purple-900/20 rounded transition-colors"
          aria-label={showForm ? "Cancel new schedule" : "Schedule new transfer"}
        >
          {showForm ? <X size={12} /> : <Plus size={12} />}
          <span>{showForm ? "Cancel" : "Schedule"}</span>
        </button>
      </div>

      {/* Schedule creation form */}
      {showForm && (
        <div className="px-3 py-3 mx-3 rounded-lg border border-purple-200 dark:border-purple-800 bg-purple-50/50 dark:bg-purple-900/10">
          <div className="space-y-2.5">
            {/* Direction */}
            <div className="flex items-center gap-2">
              <label className="text-xs text-gray-600 dark:text-gray-400 w-20">Direction</label>
              <select
                value={formData.direction}
                onChange={(e) => setFormData((d) => ({ ...d, direction: e.target.value as TransferDirection }))}
                className="flex-1 px-2 py-1 text-xs rounded border border-gray-200 dark:border-gray-600 bg-white dark:bg-gray-900"
              >
                <option value="upload">Upload</option>
                <option value="download">Download</option>
              </select>
            </div>

            {/* Local path */}
            <div className="flex items-center gap-2">
              <label className="text-xs text-gray-600 dark:text-gray-400 w-20">Local Path</label>
              <input
                type="text"
                value={formData.localPath}
                onChange={(e) => setFormData((d) => ({ ...d, localPath: e.target.value }))}
                placeholder="/path/to/local/file"
                className="flex-1 px-2 py-1 text-xs rounded border border-gray-200 dark:border-gray-600 bg-white dark:bg-gray-900 font-mono"
              />
            </div>

            {/* Remote path */}
            <div className="flex items-center gap-2">
              <label className="text-xs text-gray-600 dark:text-gray-400 w-20">Remote Path</label>
              <input
                type="text"
                value={formData.remotePath}
                onChange={(e) => setFormData((d) => ({ ...d, remotePath: e.target.value }))}
                placeholder="/remote/path/file"
                className="flex-1 px-2 py-1 text-xs rounded border border-gray-200 dark:border-gray-600 bg-white dark:bg-gray-900 font-mono"
              />
            </div>

            {/* Scheduled time */}
            <div className="flex items-center gap-2">
              <label className="text-xs text-gray-600 dark:text-gray-400 w-20">Start Time</label>
              <input
                type="datetime-local"
                value={formData.scheduledAt}
                min={minDateTime}
                onChange={(e) => setFormData((d) => ({ ...d, scheduledAt: e.target.value }))}
                className="flex-1 px-2 py-1 text-xs rounded border border-gray-200 dark:border-gray-600 bg-white dark:bg-gray-900"
              />
            </div>

            {/* Error */}
            {formError && (
              <div className="flex items-center gap-1.5 text-xs text-red-600 dark:text-red-400">
                <AlertCircle size={12} />
                <span>{formError}</span>
              </div>
            )}

            {/* Submit */}
            <div className="flex justify-end pt-1">
              <button
                type="button"
                onClick={handleSubmit}
                disabled={isSubmitting}
                className={clsx(
                  "px-3 py-1.5 text-xs font-medium rounded-md transition-colors",
                  "bg-purple-600 text-white hover:bg-purple-700",
                  "dark:bg-purple-700 dark:hover:bg-purple-600",
                  "disabled:opacity-50 disabled:cursor-not-allowed",
                )}
              >
                {isSubmitting ? "Scheduling…" : "Schedule Transfer"}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Scheduled transfers list */}
      {scheduled.length === 0 && !showForm ? (
        <div className="flex flex-col items-center py-6 px-4">
          <Clock className="w-8 h-8 text-gray-300 dark:text-gray-600 mb-2" />
          <p className="text-xs text-gray-400 dark:text-gray-500 text-center">
            No scheduled transfers
          </p>
        </div>
      ) : (
        <ul role="list" className="divide-y divide-gray-100 dark:divide-gray-800">
          {scheduled.map((item) => (
            <ScheduledRow
              key={item.id}
              item={item}
              transferInfo={transfers?.get(item.id)}
              onCancel={() => handleCancel(item.id)}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

// ── Scheduled Row ────────────────────────────────────────────────

interface ScheduledRowProps {
  item: ScheduledTransfer;
  transferInfo?: { deltaSavingsPercent?: number; totalBytes: number; compressedBytes?: number };
  onCancel: () => void;
}

function ScheduledRow({ item, transferInfo, onCancel }: ScheduledRowProps) {
  const fileName = item.localPath.split("/").pop() ?? item.localPath;
  const scheduledDate = new Date(item.scheduledAt);
  const timeUntil = formatTimeUntil(item.scheduledAt - Date.now());

  return (
    <li className="flex items-center gap-2 px-3 py-2 hover:bg-gray-50 dark:hover:bg-gray-800/50">
      <Clock className="w-4 h-4 text-purple-500 dark:text-purple-400 shrink-0" />
      <div className="flex-1 min-w-0">
        <p className="text-sm text-gray-800 dark:text-gray-200 truncate">{fileName}</p>
        <p className="text-xs text-gray-500 dark:text-gray-400 truncate">
          → {item.remotePath}
        </p>
        <div className="flex items-center gap-2 mt-0.5">
          <span className="text-[10px] text-purple-600 dark:text-purple-400">
            {scheduledDate.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}
          </span>
          <span className="text-[10px] text-gray-400 dark:text-gray-500">
            ({timeUntil})
          </span>
          {/* Delta savings indicator */}
          {transferInfo?.deltaSavingsPercent !== undefined && transferInfo.deltaSavingsPercent > 0 && (
            <span className="flex items-center gap-0.5 text-[10px] text-green-600 dark:text-green-400">
              <TrendingDown size={10} />
              {transferInfo.deltaSavingsPercent.toFixed(1)}% saved
            </span>
          )}
          {/* Compressed size indicator */}
          {transferInfo?.compressedBytes !== undefined && transferInfo.compressedBytes > 0 && (
            <span className="flex items-center gap-0.5 text-[10px] text-blue-600 dark:text-blue-400">
              <Archive size={10} />
              {formatBytes(transferInfo.totalBytes)} → {formatBytes(transferInfo.compressedBytes)}
            </span>
          )}
        </div>
      </div>
      <button
        type="button"
        onClick={onCancel}
        className="shrink-0 p-1 rounded hover:bg-red-50 dark:hover:bg-red-900/20 text-gray-400 hover:text-red-500 dark:hover:text-red-400 transition-colors"
        aria-label={`Cancel scheduled transfer: ${fileName}`}
        title="Cancel scheduled transfer"
      >
        <X size={14} />
      </button>
    </li>
  );
}

// ── Helpers ──────────────────────────────────────────────────────

function formatTimeUntil(ms: number): string {
  if (ms <= 0) return "starting soon";
  const minutes = Math.floor(ms / 60_000);
  if (minutes < 60) return `in ${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `in ${hours}h ${minutes % 60}m`;
  const days = Math.floor(hours / 24);
  return `in ${days}d ${hours % 24}h`;
}
