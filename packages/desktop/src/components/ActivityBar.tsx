import { useState } from "react";
import clsx from "clsx";
import {
  Bot,
  Files,
  GitGraph,
  Monitor,
  MoreHorizontal,
  Package,
  Settings,
  Workflow,
} from "lucide-react";
import { navigationGroup, type SidebarView } from "../lib/navigation";
import { ContextMenu } from "./ContextMenu";

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
] as const;

export function ActivityBar({
  active,
  onSelect,
  hiddenViews = [],
  badges = {},
}: Props) {
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const moreItems = [
    { id: "workflows" as const, label: "Data Workflows", icon: Workflow },
    { id: "ssh" as const, label: "SSH Connections", icon: Monitor },
  ].filter((item) => !hiddenViews.includes(item.id));
  const buttonClass =
    "relative mb-1 flex h-10 w-10 items-center justify-center rounded text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover focus-visible:outline focus-visible:outline-2 focus-visible:outline-gx-accent";
  return (
    <nav
      className="flex h-full w-12 shrink-0 flex-col items-center border-r border-gx-border bg-gx-surface py-2"
      aria-label="Primary"
    >
      {PRIMARY.map((item) => {
        const selected =
          active === item.id ||
          (navigationGroup(active) != null &&
            navigationGroup(active) === navigationGroup(item.id));
        const count =
          item.id === "changes"
            ? (badges.changes ?? 0) + (badges.timeline ?? 0)
            : (badges[item.id] ?? 0);
        return (
          <button
            key={item.id}
            type="button"
            onClick={() => onSelect(item.id)}
            className={clsx(buttonClass, selected && "text-gx-text")}
            aria-label={item.label}
            aria-pressed={selected}
            title={item.label}
          >
            <item.icon size={20} />
            {selected && (
              <span className="absolute left-0 top-1 h-8 w-px bg-gx-accent" />
            )}
            {count > 0 && (
              <span className="absolute right-0 top-0 rounded-full bg-gx-error px-1 text-[9px] text-white">
                {count > 99 ? "99+" : count}
              </span>
            )}
          </button>
        );
      })}
      <button
        type="button"
        className={clsx(
          buttonClass,
          moreItems.some((item) => item.id === active) &&
            "text-gx-text bg-gx-surface-hover",
        )}
        aria-label="More tools"
        title="More tools"
        aria-haspopup="menu"
        aria-expanded={menu != null}
        onClick={(event) => {
          const rect = event.currentTarget.getBoundingClientRect();
          setMenu(menu ? null : { x: rect.right + 4, y: rect.top });
        }}
      >
        <MoreHorizontal size={20} />
      </button>
      <div className="flex-1" />
      <button
        type="button"
        className={clsx(
          buttonClass,
          active === "settings" && "text-gx-text bg-gx-surface-hover",
        )}
        aria-label="Settings"
        title="Settings (⌘,)"
        aria-pressed={active === "settings"}
        onClick={() => onSelect("settings")}
      >
        <Settings size={20} />
      </button>
      {menu && (
        <ContextMenu
          items={moreItems}
          position={menu}
          onSelect={(id) => {
            const item = moreItems.find((entry) => entry.id === id);
            if (item) onSelect(item.id);
            setMenu(null);
          }}
          onClose={() => setMenu(null)}
        />
      )}
    </nav>
  );
}
