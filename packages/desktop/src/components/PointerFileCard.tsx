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
 * `git_stage_hunks` / `git_unstage_hunks` / `git_discard_hunks`
 * verbs (or to `git add` for the whole-file / untracked case).
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
  onDiscardFile: () => void;
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
  const label = pointerKindLabel(kind);
  // Basename for the heading — the path is shown full in the file
  // header above the card, so the card focuses on the explanation.
  const basename = path.includes("/") ? path.slice(path.lastIndexOf("/") + 1) : path;

  return (
    <div
      className={clsx(
        "flex flex-col gap-3 border-b border-gx-border bg-gx-surface p-4",
        className,
      )}
      role="region"
      aria-label={`${label} staging for ${path}`}
    >
      <div className="flex items-start gap-3">
        <div className="mt-0.5 shrink-0 rounded bg-gx-accent/15 p-2 text-gx-accent">
          <FileCode2 size={16} aria-hidden="true" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2 text-[12.5px]">
            <span className="font-medium text-gx-text">{label}</span>
            <span
              className="rounded border border-gx-border px-1.5 py-[1px] font-mono text-[10px] text-gx-muted"
              title={path}
            >
              {basename}
            </span>
          </div>
          <p className="mt-1 text-[11px] leading-relaxed text-gx-muted">
            {kind === "crab" ? "Crab pointers" : kind === "lfs" ? "LFS pointers" : "Pointers"}
            {" are hex blobs — per-hunk editing isn't supported. This stages the pointer change as a whole."}
          </p>
        </div>
      </div>
      <div
        role="toolbar"
        aria-label="Pointer file actions"
        className="flex items-center gap-1 border-t border-gx-border/60 pt-3"
      >
        <CardButton
          icon={<Plus size={12} />}
          label="Stage entire pointer change"
          onClick={onStageFile}
          tone="success"
        />
        <CardButton
          icon={<Minus size={12} />}
          label="Unstage entire pointer change"
          onClick={() => onUnstageFile?.()}
          disabled={!canUnstage || !onUnstageFile}
          tone="warn"
        />
        <CardButton
          icon={<Trash2 size={12} />}
          label="Discard entire pointer change"
          onClick={onDiscardFile}
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
      ? "hover:bg-gx-success/15 hover:text-gx-success"
      : tone === "warn"
        ? "hover:bg-gx-warn/15 hover:text-gx-warn"
        : "hover:bg-gx-error/15 hover:text-gx-error";
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-label={label}
      title={label}
      className={clsx(
        "flex items-center gap-1.5 rounded border border-transparent px-2 py-1 text-[11px] text-gx-muted transition",
        toneCls,
        disabled && "cursor-not-allowed opacity-40 hover:bg-transparent hover:text-gx-muted",
        className,
      )}
    >
      {icon}
      <span>{label}</span>
    </button>
  );
}
