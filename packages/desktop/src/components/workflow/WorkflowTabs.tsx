import clsx from "clsx";

export type WorkflowTab = "editor" | "status" | "experiments";

interface Props {
  active: WorkflowTab;
  onChange: (tab: WorkflowTab) => void;
  outdatedCount?: number;
  runningExpCount?: number;
}

export function WorkflowTabs({ active, onChange, outdatedCount, runningExpCount }: Props) {
  return (
    <div className="flex border-b border-gx-border bg-gx-surface">
      <TabButton
        label="Editor"
        active={active === "editor"}
        onClick={() => onChange("editor")}
      />
      <TabButton
        label="Status"
        active={active === "status"}
        onClick={() => onChange("status")}
        badge={outdatedCount && outdatedCount > 0 ? outdatedCount : undefined}
        badgeColor="bg-gx-warn"
      />
      <TabButton
        label="Experiments"
        active={active === "experiments"}
        onClick={() => onChange("experiments")}
        badge={runningExpCount && runningExpCount > 0 ? runningExpCount : undefined}
        badgeColor="bg-gx-info"
      />
    </div>
  );
}

function TabButton({
  label,
  active,
  onClick,
  badge,
  badgeColor,
}: {
  label: string;
  active: boolean;
  onClick: () => void;
  badge?: number;
  badgeColor?: string;
}) {
  return (
    <button
      type="button"
      className={clsx(
        "relative px-4 py-2 text-xs font-medium transition-colors",
        active
          ? "border-b-2 border-gx-accent text-gx-text"
          : "text-gx-muted hover:text-gx-text",
      )}
      onClick={onClick}
    >
      {label}
      {badge != null && (
        <span
          className={clsx(
            "ml-1.5 inline-flex h-4 min-w-4 items-center justify-center rounded-full px-1 text-[10px] font-bold text-white",
            badgeColor ?? "bg-gx-accent",
          )}
        >
          {badge}
        </span>
      )}
    </button>
  );
}
