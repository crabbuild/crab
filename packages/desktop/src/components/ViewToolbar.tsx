import { Button } from "@/components/ui/button";
import { Toggle } from "@/components/ui/toggle";
import type { LucideIcon } from "lucide-react";
import { Loader2 } from "lucide-react";

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
      className="flex min-h-11 shrink-0 flex-wrap items-center justify-between gap-2 border-b border-border bg-card px-3 py-1.5"
      role="toolbar"
      aria-label={title}
    >
      <div className="flex items-center gap-1.5 text-xs font-medium text-foreground">
        {Icon && <Icon size={16} className="text-muted-foreground" />}
        <span>{title}</span>
      </div>

      <div className="flex flex-wrap items-center gap-1">
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

  const icon = loading ? <Loader2 className="animate-spin" /> : <ActionIcon />;
  if (active !== undefined)
    return (
      <Toggle
        title={tooltip}
        aria-label={tooltip}
        disabled={disabled || loading}
        pressed={active}
        onPressedChange={() => onAction(id)}
        size="sm"
        className="[&_svg]:size-4"
      >
        {icon}
      </Toggle>
    );
  return (
    <Button
      variant="ghost"
      size="icon-sm"
      type="button"
      title={tooltip}
      aria-label={tooltip}
      disabled={disabled || loading}
      onClick={() => onAction(id)}
    >
      {icon}
    </Button>
  );
}
