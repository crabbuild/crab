/**
 * Write-blocked overlay for SSH reconnection.
 *
 * Displayed over write-operation UI elements (commit button, push
 * button, stage actions, etc.) when the SSH connection is in the
 * "reconnecting" state. Shows a spinner and "Waiting for
 * connection..." message to indicate the operation will proceed
 * once the connection is restored.
 */

import clsx from "clsx";
import { Loader2, WifiOff } from "lucide-react";

interface WriteBlockedOverlayProps {
  /** Whether the overlay is visible. */
  visible: boolean;
  /** Optional message override. */
  message?: string;
  /** Inline mode — renders as a compact inline element instead of a full overlay. */
  inline?: boolean;
  className?: string;
}

export function WriteBlockedOverlay({
  visible,
  message = "Waiting for connection\u2026",
  inline = false,
  className,
}: WriteBlockedOverlayProps) {
  if (!visible) return null;

  if (inline) {
    return (
      <span
        className={clsx(
          "inline-flex items-center gap-1.5 text-xs text-muted-foreground text-muted-foreground",
          className,
        )}
        role="status"
        aria-live="polite"
        aria-label={message}
      >
        <Loader2 className="w-3 h-3 animate-spin" aria-hidden="true" />
        <span>{message}</span>
      </span>
    );
  }

  return (
    <div
      className={clsx(
        "absolute inset-0 z-10 flex flex-col items-center justify-center",
        "bg-background/80 bg-background/80 backdrop-blur-[1px]",
        "rounded-md",
        className,
      )}
      role="status"
      aria-live="polite"
      aria-label={message}
    >
      <div className="flex flex-col items-center gap-2">
        <div className="relative">
          <WifiOff className="w-5 h-5 text-muted-foreground text-muted-foreground" aria-hidden="true" />
          <Loader2
            className="absolute -top-1 -right-1 w-3 h-3 animate-spin text-info"
            aria-hidden="true"
          />
        </div>
        <span className="text-sm text-muted-foreground text-muted-foreground font-medium">
          {message}
        </span>
        <span className="text-xs text-muted-foreground text-muted-foreground">
          Reconnecting to server
        </span>
      </div>
    </div>
  );
}
