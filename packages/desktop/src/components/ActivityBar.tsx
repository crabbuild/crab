import { Toggle } from "@/components/ui/toggle";
import clsx from "clsx";
import {
  Bot,
  Files,
  GitGraph,
  Monitor,
  Package,
  Settings,
  Terminal,
  Workflow,
} from "lucide-react";
import { navigationGroup, type SidebarView } from "../lib/navigation";

interface Props {
  active: SidebarView;
  onSelect: (view: SidebarView) => void;
  hiddenViews?: SidebarView[];
  badges?: Partial<Record<SidebarView, number>>;
}

const PRIMARY = [
  { id: "orchestrator", label: "Chat", icon: Bot },
  { id: "explorer", label: "Files", icon: Files },
  { id: "changes", label: "Git", icon: GitGraph },
  { id: "crab", label: "Large Files", icon: Package },
  { id: "workflows", label: "Data Workflows", icon: Workflow },
  { id: "ssh", label: "SSH Connections", icon: Monitor },
  { id: "terminal", label: "Terminal", icon: Terminal },
] as const;

export function ActivityBar({
  active,
  onSelect,
  hiddenViews = [],
  badges = {},
}: Props) {
  const buttonClass =
    "relative mb-1 flex h-10 w-10 items-center justify-center rounded text-muted-foreground hover:text-foreground hover:bg-accent focus-visible:outline focus-visible:outline-2 focus-visible:outline-primary";
  return (
    <nav
      className="flex h-full w-[var(--activity-bar-width)] shrink-0 flex-col items-center border-r border-border bg-card py-2"
      aria-label="Primary"
    >
      {PRIMARY.filter((item) => !hiddenViews.includes(item.id)).map((item) => {
        const selected =
          active === item.id ||
          (navigationGroup(active) != null &&
            navigationGroup(active) === navigationGroup(item.id));
        const count =
          item.id === "changes"
            ? (badges.changes ?? 0) + (badges.timeline ?? 0)
            : (badges[item.id] ?? 0);
        return (
          <Toggle
            size="sm"
            key={item.id}
            type="button"
            onClick={() => onSelect(item.id)}
            className={clsx(buttonClass, selected && "text-foreground")}
            aria-label={item.label}
            pressed={selected}
            title={item.label}
          >
            <item.icon size={20} />
            {selected && (
              <span className="absolute left-0 top-1 h-8 w-px bg-primary" />
            )}
            {count > 0 && (
              <span className="absolute right-0 top-0 rounded-full bg-destructive px-1 text-[9px] text-destructive-foreground">
                {count > 99 ? "99+" : count}
              </span>
            )}
          </Toggle>
        );
      })}
      <div className="flex-1" />
      <Toggle
        size="sm"
        type="button"
        className={clsx(
          buttonClass,
          active === "settings" && "text-foreground bg-accent",
        )}
        aria-label="Settings"
        title="Settings (⌘,)"
        pressed={active === "settings"}
        onClick={() => onSelect("settings")}
      >
        <Settings size={20} />
      </Toggle>
    </nav>
  );
}
