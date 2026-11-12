import { Button } from "@/components/ui/button";
import { Calendar } from "@/components/ui/calendar";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { format, startOfDay } from "date-fns";
/**
 * TransferScheduleSection — displays scheduled transfers and allows
 * creating/cancelling scheduled transfers.
 *
 * Shows file name, destination, planned start time, delta savings,
 * and compressed sizes. Validates that scheduled times are in the future.
 */

import clsx from "clsx";
import {
  AlertCircle,
  Archive,
  Calendar as CalendarIcon,
  Clock,
  Plus,
  TrendingDown,
  X,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";
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
  scheduledAt: string; // Local date and time, without a timezone suffix.
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
  const selectedDate = formData.scheduledAt
    ? new Date(formData.scheduledAt)
    : undefined;
  const selectedTime = formData.scheduledAt.slice(11) || "00:00";

  return (
    <div className="flex flex-col gap-3" role="region" aria-label="Scheduled transfers">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-border border-border">
        <div className="flex items-center gap-2">
          <Calendar className="w-4 h-4 text-thinking text-thinking" />
          <h3 className="text-xs font-semibold text-muted-foreground text-muted-foreground">
            Scheduled Transfers
          </h3>
          {scheduled.length > 0 && (
            <span className="text-xs text-muted-foreground text-muted-foreground">
              ({scheduled.length})
            </span>
          )}
        </div>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setShowForm(!showForm)}
          className="flex items-center gap-1 px-2 py-1 text-xs font-medium text-thinking text-thinking hover:bg-thinking/10 hover:bg-thinking/20 rounded transition-colors"
          aria-label={showForm ? "Cancel new schedule" : "Schedule new transfer"}
        >
          {showForm ? <X size={12} /> : <Plus size={12} />}
          <span>{showForm ? "Cancel" : "Schedule"}</span>
        </Button>
      </div>

      {/* Schedule creation form */}
      {showForm && (
        <div className="px-3 py-3 mx-3 rounded-lg border border-thinking border-thinking bg-thinking/50 bg-thinking/10">
          <div className="space-y-2.5">
            {/* Direction */}
            <div className="flex items-center gap-2">
              <label className="text-xs text-muted-foreground text-muted-foreground w-20">
                Direction
              </label>
              <Select
                value={":" + String(formData.direction)}
                onValueChange={(selectedValue) => {
                  const value = selectedValue.slice(1);
                  return setFormData((d) => ({
                    ...d,
                    direction: value as TransferDirection,
                  }));
                }}
              >
                <SelectTrigger className="flex-1 px-2 py-1 text-xs rounded border border-border border-border bg-background bg-background">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectGroup>
                    <SelectItem value={":" + String("upload")}>
                      Upload
                    </SelectItem>
                    <SelectItem value={":" + String("download")}>
                      Download
                    </SelectItem>
                  </SelectGroup>
                </SelectContent>
              </Select>
            </div>

            {/* Local path */}
            <div className="flex items-center gap-2">
              <label className="text-xs text-muted-foreground text-muted-foreground w-20">
                Local Path
              </label>
              <Input
                type="text"
                value={formData.localPath}
                onChange={(e) => setFormData((d) => ({ ...d, localPath: e.target.value }))}
                placeholder="/path/to/local/file"
                className="flex-1 px-2 py-1 text-xs rounded border border-border border-border bg-background bg-background font-mono"
              />
            </div>

            {/* Remote path */}
            <div className="flex items-center gap-2">
              <label className="text-xs text-muted-foreground text-muted-foreground w-20">
                Remote Path
              </label>
              <Input
                type="text"
                value={formData.remotePath}
                onChange={(e) => setFormData((d) => ({ ...d, remotePath: e.target.value }))}
                placeholder="/remote/path/file"
                className="flex-1 px-2 py-1 text-xs rounded border border-border border-border bg-background bg-background font-mono"
              />
            </div>

            {/* Scheduled time */}
            <div className="flex items-center gap-2">
              <label className="text-xs text-muted-foreground text-muted-foreground w-20">
                Start Time
              </label>
              <div className="flex min-w-0 flex-1 items-center gap-1">
                <Popover>
                  <PopoverTrigger asChild>
                    <Button
                      className="min-w-0 flex-1 justify-start text-xs"
                      aria-label="Scheduled date"
                    >
                      <CalendarIcon size={12} />
                      {selectedDate
                        ? format(selectedDate, "PP")
                        : "Pick a date"}
                    </Button>
                  </PopoverTrigger>
                  <PopoverContent className="w-auto p-0">
                    <Calendar
                      mode="single"
                      selected={selectedDate}
                      disabled={{ before: startOfDay(new Date()) }}
                      initialFocus
                      onSelect={(date) => {
                        if (date)
                          setFormData((data) => ({
                            ...data,
                            scheduledAt: `${format(date, "yyyy-MM-dd")}T${selectedTime}`,
                          }));
                      }}
                    />
                  </PopoverContent>
                </Popover>
                <Select
                  disabled={!selectedDate}
                  value={selectedTime.slice(0, 2)}
                  onValueChange={(hour) =>
                    setFormData((data) => ({
                      ...data,
                      scheduledAt: `${data.scheduledAt.slice(0, 11)}${hour}:${selectedTime.slice(3)}`,
                    }))
                  }
                >
                  <SelectTrigger aria-label="Scheduled hour" className="w-16">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectGroup>
                      {Array.from({ length: 24 }, (_, hour) =>
                        String(hour).padStart(2, "0"),
                      ).map((hour) => (
                        <SelectItem key={hour} value={hour}>
                          {hour}
                        </SelectItem>
                      ))}
                    </SelectGroup>
                  </SelectContent>
                </Select>
                <span>:</span>
                <Select
                  disabled={!selectedDate}
                  value={selectedTime.slice(3)}
                  onValueChange={(minute) =>
                    setFormData((data) => ({
                      ...data,
                      scheduledAt: `${data.scheduledAt.slice(0, 11)}${selectedTime.slice(0, 2)}:${minute}`,
                    }))
                  }
                >
                  <SelectTrigger aria-label="Scheduled minute" className="w-16">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectGroup>
                      {Array.from({ length: 60 }, (_, minute) =>
                        String(minute).padStart(2, "0"),
                      ).map((minute) => (
                        <SelectItem key={minute} value={minute}>
                          {minute}
                        </SelectItem>
                      ))}
                    </SelectGroup>
                  </SelectContent>
                </Select>
              </div>
            </div>

            {/* Error */}
            {formError && (
              <div className="flex items-center gap-1.5 text-xs text-destructive text-destructive">
                <AlertCircle size={12} />
                <span>{formError}</span>
              </div>
            )}

            {/* Submit */}
            <div className="flex justify-end pt-1">
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={handleSubmit}
                disabled={isSubmitting}
                className={clsx(
                  "px-3 py-1.5 text-xs font-medium rounded-md transition-colors",
                  "bg-thinking text-thinking-foreground hover:bg-thinking",
                  "bg-thinking hover:bg-thinking",
                  "disabled:opacity-50 disabled:cursor-not-allowed",
                )}
              >
                {isSubmitting ? "Scheduling…" : "Schedule Transfer"}
              </Button>
            </div>
          </div>
        </div>
      )}

      {/* Scheduled transfers list */}
      {scheduled.length === 0 && !showForm ? (
        <div className="flex flex-col items-center py-6 px-4">
          <Clock className="w-8 h-8 text-muted-foreground text-muted-foreground mb-2" />
          <p className="text-xs text-muted-foreground text-muted-foreground text-center">
            No scheduled transfers
          </p>
        </div>
      ) : (
        <ul role="list" className="divide-y divide-border divide-border">
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
    <li className="flex items-center gap-2 px-3 py-2 hover:bg-background hover:bg-card/50">
      <Clock className="w-4 h-4 text-thinking text-thinking shrink-0" />
      <div className="flex-1 min-w-0">
        <p className="text-sm text-foreground text-foreground truncate">
          {fileName}
        </p>
        <p className="text-xs text-muted-foreground text-muted-foreground truncate">
          → {item.remotePath}
        </p>
        <div className="flex items-center gap-2 mt-0.5">
          <span className="text-[10px] text-thinking text-thinking">
            {scheduledDate.toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}
          </span>
          <span className="text-[10px] text-muted-foreground text-muted-foreground">
            ({timeUntil})
          </span>
          {/* Delta savings indicator */}
          {transferInfo?.deltaSavingsPercent !== undefined && transferInfo.deltaSavingsPercent > 0 && (
              <span className="flex items-center gap-0.5 text-[10px] text-success text-success">
                <TrendingDown size={10} />
                {transferInfo.deltaSavingsPercent.toFixed(1)}% saved
              </span>
            )}
          {/* Compressed size indicator */}
          {transferInfo?.compressedBytes !== undefined && transferInfo.compressedBytes > 0 && (
              <span className="flex items-center gap-0.5 text-[10px] text-info text-info">
                <Archive size={10} />
                {formatBytes(transferInfo.totalBytes)} →{" "}
                {formatBytes(transferInfo.compressedBytes)}
              </span>
            )}
        </div>
      </div>
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={onCancel}
        className="shrink-0 p-1 rounded hover:bg-destructive/10 hover:bg-destructive/20 text-muted-foreground hover:text-destructive hover:text-destructive transition-colors"
        aria-label={`Cancel scheduled transfer: ${fileName}`}
        title="Cancel scheduled transfer"
      >
        <X size={14} />
      </Button>
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
