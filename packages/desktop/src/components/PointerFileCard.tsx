import clsx from "clsx";
import { FileCode2, Minus, Plus, Trash2 } from "lucide-react";
import { PointerPatchKind, pointerKindLabel } from "../lib/pointer-detection";

/**
 * Whole-file staging affordance for Crab / LFS pointer blobs.
 *
 * Pointer files store content-addressed hex — the "lines" in a
 * pointer blob are metadata, not something the user can sensibly
 * stage or discard piecemeal. When `<InlinePatchDiff />` detects
 * a pointer signature on either side of a file's patch, it swaps
 * the usual per-hunk toolbar for this single card: three buttons
 * that stage / unstage / discard the *entire* pointer change at
 * once.
 *
 * The card emits intents only; the consumer wires them up to the
 * whole-file staging and confirmed, verified index restoration owners.
 *
 * Visual language matches `<HunkToolbar />`: same tone classes,
 * same icon set, same confirm-before-discard flow. The card is
 * deliberately larger than the toolbar so users notice the
 * affordance swap — a Crab pointer edit is not a normal text
 * edit and the UI should communicate that.
 */
export interface PointerFileCardProps {
  /** Repo-relative path of the pointer file. Rendered as the title. */
  path: string;
  /** Which pointer flavour was detected. `null` is technically
   *  invalid — the parent shouldn't render this card without a
   *  detected kind — but we tolerate it and render as a generic
   *  "Pointer" rather than blowing up. */
  kind: PointerPatchKind | null;
  /** Show the Unstage button as enabled. The parent derives this
   *  from the file's tri-state (any staged content = unstageable). */
  canUnstage?: boolean;
  onStageFile: () => void;
  onUnstageFile?: () => void;
  onDiscardFile?: () => void;
  className?: string;
}

export function PointerFileCard({
  path,
  kind,
  canUnstage = false,
  onStageFile,
  onUnstageFile,
  onDiscardFile,
  className,
}: PointerFileCardProps) {
  const label = kind ? pointerKindLabel(kind) : "Binary or mode change";
  // Basename for the heading — the path is shown full in the file
  // header above the card, so the card focuses on the explanation.
  const basename = path.includes("/") ? path.slice(path.lastIndexOf("/") + 1) : path;

  return (
    <div
      className={clsx(
        "flex flex-col gap-3 border-b border-border bg-card p-4",
        className,
      )}
      role="region"
      aria-label={`${label} staging for ${path}`}
    >
      <div className="flex items-start gap-3">
        <div className="mt-0.5 shrink-0 rounded bg-primary/15 p-2 text-primary">
          <FileCode2 size={16} aria-hidden="true" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2 text-[12.5px]">
            <span className="font-medium text-foreground">{label}</span>
            <span
              className="rounded border border-border px-1.5 py-[1px] font-mono text-[10px] text-muted-foreground"
              title={path}
            >
              {basename}
            </span>
          </div>
          <p className="mt-1 text-[11px] leading-relaxed text-muted-foreground">
            This change has whole-file actions. Discard restores the index version after confirmation.
          </p>
        </div>
      </div>
      <div
        role="toolbar"
        aria-label="Whole-file actions"
        className="flex items-center gap-1 border-t border-border/60 pt-3"
      >
        <CardButton
          icon={<Plus size={12} />}
          label={kind ? "Stage entire pointer change" : "Stage entire file change"}
          onClick={onStageFile}
          tone="success"
        />
        <CardButton
          icon={<Minus size={12} />}
          label={kind ? "Unstage entire pointer change" : "Unstage entire file change"}
          onClick={() => onUnstageFile?.()}
          disabled={!canUnstage || !onUnstageFile}
          tone="warn"
        />
        <CardButton
          icon={<Trash2 size={12} />}
          label={kind ? "Discard entire pointer change" : "Discard entire file change"}
          onClick={() => onDiscardFile?.()}
          disabled={!onDiscardFile}
          tone="error"
          className="ml-auto"
        />
      </div>
    </div>
  );
}

function CardButton({
  icon,
  label,
  onClick,
  disabled,
  tone,
  className,
}: {
  icon: React.ReactNode;
  label: string;
  onClick: () => void;
  disabled?: boolean;
  tone: "success" | "warn" | "error";
  className?: string;
}) {
  const toneCls =
    tone === "success"
      ? "hover:bg-success/15 hover:text-success"
      : tone === "warn"
        ? "hover:bg-warning/15 hover:text-warning"
        : "hover:bg-destructive/15 hover:text-destructive";
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-label={label}
      title={label}
      className={clsx(
        "flex items-center gap-1.5 rounded border border-transparent px-2 py-1 text-[11px] text-muted-foreground transition",
        toneCls,
        disabled && "cursor-not-allowed opacity-40 hover:bg-transparent hover:text-muted-foreground",
        className,
      )}
    >
      {icon}
      <span>{label}</span>
    </button>
  );
}
