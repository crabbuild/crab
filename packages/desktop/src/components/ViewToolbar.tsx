import type { LucideIcon } from "lucide-react";
import { Loader2 } from "lucide-react";
import clsx from "clsx";

export interface ToolbarAction {
  id: string;
  icon: LucideIcon;
  label: string;
  shortcut?: string;
  disabled?: boolean;
  /** When true, shows a spinner instead of the icon. */
  loading?: boolean;
  /** Toggle state for toggle-style buttons (e.g., Amend). */
  active?: boolean;
}

export interface ViewToolbarProps {
  /** The view title (e.g., "Changes", "Branches"). */
  title: string;
  /** Icon for the view title. */
  icon?: LucideIcon;
  /** Toolbar actions rendered as icon buttons. */
  actions: ToolbarAction[];
  /** Called when an action button is clicked. */
  onAction: (actionId: string) => void;
  /** Optional right-side content (e.g., filter input, count badge). */
  trailing?: React.ReactNode;
}

/**
 * Reusable view header toolbar. Renders a horizontal bar with the view
 * icon + title on the left and action icon buttons on the right. Used
 * across Changes, Branches, Stashes, Worktrees, Conflicts, Large Files,
 * and Pull Requests views.
 */
export function ViewToolbar({ title, icon: Icon, actions, onAction, trailing }: ViewToolbarProps) {
  return (
    <div
      className="flex h-8 shrink-0 items-center justify-between border-b border-border bg-card px-2"
      role="toolbar"
      aria-label={title}
    >
      <div className="flex items-center gap-1.5 text-xs font-medium text-foreground">
        {Icon && <Icon size={14} className="text-muted-foreground" />}
        <span>{title}</span>
      </div>

      <div className="flex items-center gap-[4px]">
        {actions.map((action) => (
          <ActionButton key={action.id} action={action} onAction={onAction} />
        ))}
        {trailing}
      </div>
    </div>
  );
}

function ActionButton({
  action,
  onAction,
}: {
  action: ToolbarAction;
  onAction: (actionId: string) => void;
}) {
  const { id, icon: ActionIcon, label, shortcut, disabled, loading, active } = action;

  const tooltip = shortcut ? `${label} (${shortcut})` : label;

  return (
    <button
      type="button"
      title={tooltip}
      aria-label={tooltip}
      disabled={disabled || loading}
      onClick={() => onAction(id)}
      className={clsx(
        "flex h-6 w-6 items-center justify-center rounded text-muted-foreground transition-colors",
        "hover:bg-primary/20 hover:text-foreground",
        active && "bg-primary/20 text-foreground",
        disabled && "cursor-not-allowed opacity-50 hover:bg-transparent hover:text-muted-foreground",
        loading && "cursor-not-allowed opacity-50",
      )}
    >
      {loading ? (
        <Loader2 size={14} className="animate-spin" />
      ) : (
        <ActionIcon size={14} />
      )}
    </button>
  );
}
