"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { Check, Clipboard, X } from "lucide-react";
import { cn } from "@/lib/utils";

type CopyState = "idle" | "copied" | "error";

interface CopyButtonProps {
  containerRef: React.RefObject<HTMLElement | null>;
  className?: string;
}

/**
 * Copy-to-clipboard button for code blocks.
 * Shows "Copied!" tooltip for 2s on success, or an error indicator on failure.
 */
export function CopyButton({ containerRef, className, ...props }: CopyButtonProps) {
  const [state, setState] = useState<CopyState>("idle");
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearTimer = useCallback(() => {
    if (timeoutRef.current) {
      clearTimeout(timeoutRef.current);
      timeoutRef.current = null;
    }
  }, []);

  useEffect(() => {
    return () => clearTimer();
  }, [clearTimer]);

  const onClick = useCallback(async () => {
    clearTimer();

    const pre = containerRef.current?.getElementsByTagName("pre").item(0);
    if (!pre) {
      setState("error");
      timeoutRef.current = setTimeout(() => setState("idle"), 2000);
      return;
    }

    const clone = pre.cloneNode(true) as HTMLElement;
    clone.querySelectorAll(".nd-copy-ignore").forEach((node) => {
      node.replaceWith("\n");
    });

    const text = clone.textContent ?? "";

    try {
      if (!navigator.clipboard) {
        throw new Error("Clipboard API unavailable");
      }
      await navigator.clipboard.writeText(text);
      setState("copied");
    } catch {
      setState("error");
    }

    timeoutRef.current = setTimeout(() => setState("idle"), 2000);
  }, [containerRef, clearTimer]);

  const ariaLabel =
    state === "copied"
      ? "Copied!"
      : state === "error"
        ? "Copy failed"
        : "Copy to clipboard";

  return (
    <button
      type="button"
      data-checked={state === "copied" || undefined}
      data-error={state === "error" || undefined}
      className={cn(
        "inline-flex items-center justify-center size-7 rounded-md transition-colors",
        "text-fd-muted-foreground hover:text-fd-accent-foreground",
        "data-checked:text-green-500 data-error:text-red-500",
        "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-fd-ring",
        className
      )}
      aria-label={ariaLabel}
      title={ariaLabel}
      onClick={onClick}
      {...props}
    >
      {state === "copied" ? (
        <Check className="size-3.5" />
      ) : state === "error" ? (
        <X className="size-3.5" />
      ) : (
        <Clipboard className="size-3.5" />
      )}
    </button>
  );
}
