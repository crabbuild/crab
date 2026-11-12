import { Button } from "@/components/ui/button";
import { Check, Clipboard, FileCode2 } from "lucide-react";
import { useCallback, useState } from "react";
import { PointerPatchKind, pointerKindLabel } from "../lib/pointer-detection";

/**
 * Shown in place of the 3-pane Monaco merge editor when a conflict
 * involves a Crab or LFS pointer file.
 *
 * Pointer blobs are hex-encoded metadata — the merge editor cannot
 * meaningfully diff or merge binary content referenced by pointers.
 * This card explains the situation and offers a "Copy hydrate
 * command" button so the user can materialise the file and resolve
 * the conflict manually.
 */
export interface PointerConflictCardProps {
  /** Repo-relative path of the conflicting pointer file. */
  path: string;
  /** Which pointer flavour was detected. */
  kind: PointerPatchKind | null;
}

export function PointerConflictCard({ path, kind }: PointerConflictCardProps) {
  const [copied, setCopied] = useState(false);
  const label = pointerKindLabel(kind);
  const hydrateCommand = `crab hydrate ${path}`;

  const handleCopy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(hydrateCommand);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard API may fail in some contexts; silently ignore.
    }
  }, [hydrateCommand]);

  return (
    <div
      className="mx-auto my-8 flex max-w-lg flex-col gap-4 rounded-lg border border-border bg-card p-6 shadow-sm"
      role="region"
      aria-label={`${label} conflict notice for ${path}`}
    >
      <div className="flex items-start gap-3">
        <div className="mt-0.5 shrink-0 rounded bg-warning/15 p-2.5 text-warning">
          <FileCode2 size={20} aria-hidden="true" />
        </div>
        <div className="min-w-0 flex-1">
          <h3 className="text-sm font-medium text-foreground">
            {label} — cannot merge inline
          </h3>
          <p className="mt-1.5 text-xs leading-relaxed text-muted-foreground">
            This file is tracked as a{" "}
            {kind === "crab" ? "Crab" : kind === "lfs" ? "Git LFS" : "pointer"}{" "}
            pointer. The merge editor cannot compare binary content. Run the
            hydrate command below to materialise the file, then resolve the
            conflict manually.
          </p>
        </div>
      </div>

      {/* Hydrate command block */}
      <div className="flex items-center gap-2 rounded border border-border bg-background px-3 py-2">
        <code className="flex-1 select-all font-mono text-xs text-foreground">
          {hydrateCommand}
        </code>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={handleCopy}
          className="flex shrink-0 items-center gap-1.5 rounded border border-border px-2 py-1 text-[11px] text-muted-foreground transition hover:bg-accent hover:text-foreground"
          aria-label="Copy hydrate command"
          title="Copy hydrate command"
        >
          {copied ? (
            <>
              <Check size={12} className="text-success" />
              <span className="text-success">Copied</span>
            </>
          ) : (
            <>
              <Clipboard size={12} />
              <span>Copy</span>
            </>
          )}
        </Button>
      </div>

      <p className="text-[11px] leading-relaxed text-muted-foreground">
        After resolving, paste the final content into the result pane below and
        click <strong className="text-foreground">Save Resolution</strong>.
      </p>
    </div>
  );
}
