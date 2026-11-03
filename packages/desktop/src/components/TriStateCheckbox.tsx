import clsx from "clsx";
import { useCallback } from "react";

/**
 * Three-state staging checkbox for a file in the Changes view.
 *
 * Rendered as a tight 14×14 square to fit inline with a list row
 * without pushing the filename down. The three visual states map
 * 1:1 to the derivation in `lib/file-state.ts`:
 *
 *   - `empty`   — empty outline
 *   - `partial` — filled square with a small centered dash, the
 *                 VS Code convention for "indeterminate"
 *   - `full`    — filled square with a centered check
 *
 * Behaviour follows the WAI-ARIA Authoring Practices tri-state
 * checkbox pattern: `role="checkbox"`, `aria-checked="true" |
 * "false" | "mixed"`, Space toggles, keyboard focus visible. The
 * component is *stateless*: parent drives `state` and handles the
 * toggle. On click, we `stopPropagation` so clicking the checkbox
 * doesn't also trigger the enclosing row's selection handler.
 *
 * Toggle semantics (the caller's choice, surfaced here as the
 * `onToggle(nextIntent)` callback) — when the user clicks:
 *   - `empty`   → `stage`   (stage everything remaining for this file)
 *   - `partial` → `stage`   (finish staging the rest)
 *   - `full`    → `unstage` (unstage the file entirely)
 *
 * This matches VS Code's SCM checkbox UX and avoids a three-way
 * toggle loop the user can't escape with the keyboard.
 */
export type TriStateValue = "empty" | "partial" | "full";
export type TriStateIntent = "stage" | "unstage";

export interface TriStateCheckboxProps {
  state: TriStateValue;
  /** Visible / screen-reader label (e.g. "Stage src/foo.ts"). */
  label: string;
  /** Called with the *intent* implied by clicking from the current
   *  state. Parent decides how to act — the component only knows
   *  the mapping `empty|partial → stage`, `full → unstage`. */
  onToggle: (intent: TriStateIntent) => void;
  /** Render-state disabled (e.g. while a staging op is in flight).
   *  A disabled checkbox still announces its `aria-checked`
   *  mixed/true/false state so assistive tech sees the current
   *  progress even if the user can't interact. */
  disabled?: boolean;
  className?: string;
}

export function TriStateCheckbox({
  state,
  label,
  onToggle,
  disabled = false,
  className,
}: TriStateCheckboxProps) {
  const onActivate = useCallback(() => {
    if (disabled) return;
    const intent: TriStateIntent = state === "full" ? "unstage" : "stage";
    onToggle(intent);
  }, [disabled, state, onToggle]);

  const onClick = useCallback(
    (e: React.MouseEvent<HTMLSpanElement>) => {
      // Row-level click handlers (file selection) mount on the
      // enclosing `<button>`. We want the checkbox to act on its
      // own; stop the event so selection doesn't race with staging.
      e.stopPropagation();
      onActivate();
    },
    [onActivate],
  );

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLSpanElement>) => {
      // Spacebar toggles, matching the ARIA checkbox pattern. We
      // don't implement Enter → activate on purpose: rows consume
      // Enter as "open file" in the parent list, and we'd rather
      // not shadow that.
      if (e.key === " " || e.key === "Spacebar") {
        e.preventDefault();
        e.stopPropagation();
        onActivate();
      }
    },
    [onActivate],
  );

  const ariaChecked: "true" | "false" | "mixed" =
    state === "full" ? "true" : state === "partial" ? "mixed" : "false";

  return (
    <span
      role="checkbox"
      aria-checked={ariaChecked}
      aria-label={label}
      aria-disabled={disabled || undefined}
      tabIndex={disabled ? -1 : 0}
      onClick={onClick}
      onKeyDown={onKeyDown}
      className={clsx(
        "group/cb inline-flex h-[14px] w-[14px] shrink-0 items-center justify-center",
        "rounded-[2px] border transition focus-visible:outline-none",
        "focus-visible:ring-1 focus-visible:ring-gx-accent focus-visible:ring-offset-1 focus-visible:ring-offset-gx-surface",
        state === "empty"
          ? "border-gx-border bg-transparent hover:border-gx-text"
          : "border-gx-accent bg-gx-accent",
        disabled && "cursor-not-allowed opacity-50",
        !disabled && "cursor-pointer",
        className,
      )}
      data-state={state}
    >
      {state === "full" && <Check />}
      {state === "partial" && <Dash />}
      {/* `empty` renders nothing inside — the outline is the affordance. */}
    </span>
  );
}

// Inline SVGs so the checkbox has no dependency on the icon font.
// Sized to 10×10 so there's 2px of padding on each side of the
// 14×14 square.

function Check() {
  return (
    <svg
      width="10"
      height="10"
      viewBox="0 0 10 10"
      fill="none"
      aria-hidden="true"
      focusable="false"
      className="text-white"
    >
      <path
        d="M1.5 5.2 L4 7.7 L8.5 2.8"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function Dash() {
  return (
    <svg
      width="10"
      height="10"
      viewBox="0 0 10 10"
      fill="none"
      aria-hidden="true"
      focusable="false"
      className="text-white"
    >
      <path
        d="M2 5 L8 5"
        stroke="currentColor"
        strokeWidth="1.8"
        strokeLinecap="round"
      />
    </svg>
  );
}
