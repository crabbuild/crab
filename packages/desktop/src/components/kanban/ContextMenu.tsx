import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import clsx from "clsx";
import {
  ChevronsUp,
  ChevronUp,
  Minus,
  ChevronDown,
  Circle,
  Copy,
  Bot,
  Trash2,
  Users,
  Columns,
  ChevronRight,
  Workflow,
} from "lucide-react";
import type {
  AgentWorkflowDefinition,
  KanbanCardInfo,
  KanbanCardPriority,
  KanbanColumnInfo,
} from "../../ipc";

export interface ContextMenuProps {
  position: { x: number; y: number };
  targetCards: KanbanCardInfo[];
  columns: KanbanColumnInfo[];
  onSetPriority: (priority: KanbanCardPriority) => void;
  onAssign: (assignee: string) => void;
  onMoveToColumn: (columnId: string) => void;
  onCopyId: () => void;
  onRunAgent: () => void;
  agentWorkflows?: AgentWorkflowDefinition[];
  onRunWorkflow?: (definitionId: string) => void;
  onDelete: () => void;
  onClose: () => void;
}

type SubmenuType = "priority" | "assign" | "move" | "workflow" | null;

const PRIORITIES: Array<{ value: KanbanCardPriority; label: string; shortcut: string }> = [
  { value: "critical", label: "Critical", shortcut: "1" },
  { value: "high", label: "High", shortcut: "2" },
  { value: "medium", label: "Medium", shortcut: "3" },
  { value: "low", label: "Low", shortcut: "4" },
  { value: "none", label: "None", shortcut: "0" },
];

function PriorityIcon({ priority }: { priority: KanbanCardPriority }) {
  switch (priority) {
    case "critical":
      return <ChevronsUp size={12} className="text-gx-error" />;
    case "high":
      return <ChevronUp size={12} className="text-gx-warn" />;
    case "medium":
      return <Minus size={12} className="text-yellow-400" />;
    case "low":
      return <ChevronDown size={12} className="text-gx-info" />;
    case "none":
      return <Circle size={10} className="text-gx-muted" />;
  }
}

/**
 * Portal-rendered right-click context menu for kanban cards.
 * Supports bulk operations when multiple cards are selected.
 * Flips position when near viewport edges.
 */
export function ContextMenu({
  position,
  targetCards,
  columns,
  onSetPriority,
  onAssign,
  onMoveToColumn,
  onCopyId,
  onRunAgent,
  agentWorkflows = [],
  onRunWorkflow,
  onDelete,
  onClose,
}: ContextMenuProps) {
  const menuRef = useRef<HTMLDivElement>(null);
  const [adjustedPosition, setAdjustedPosition] = useState(position);
  const [activeSubmenu, setActiveSubmenu] = useState<SubmenuType>(null);
  const [submenuPosition, setSubmenuPosition] = useState<"right" | "left">("right");

  const isBulk = targetCards.length > 1;
  const isWorkflowManaged = targetCards.some(
    (card) => card.workflowProjection?.managed === true || isActiveSymphonyRuntime(card),
  );

  // Collect unique assignees from target cards for the assign submenu
  const allAssignees = Array.from(
    new Set(targetCards.map((c) => c.assignee).filter(Boolean) as string[]),
  );

  // Viewport edge detection and position flipping
  useLayoutEffect(() => {
    if (!menuRef.current) return;
    const rect = menuRef.current.getBoundingClientRect();
    const viewportWidth = window.innerWidth;
    const viewportHeight = window.innerHeight;

    let x = position.x;
    let y = position.y;

    // Flip horizontally if menu would overflow right edge
    if (x + rect.width > viewportWidth) {
      x = Math.max(0, x - rect.width);
    }

    // Flip vertically if menu would overflow bottom edge
    if (y + rect.height > viewportHeight) {
      y = Math.max(0, y - rect.height);
    }

    // Determine submenu direction
    if (x + rect.width + 200 > viewportWidth) {
      setSubmenuPosition("left");
    } else {
      setSubmenuPosition("right");
    }

    setAdjustedPosition({ x, y });
  }, [position]);

  // Close on Escape
  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      }
    }
    document.addEventListener("keydown", handleKeyDown, true);
    return () => document.removeEventListener("keydown", handleKeyDown, true);
  }, [onClose]);

  // Close on outside click
  useEffect(() => {
    function handleMouseDown(e: MouseEvent) {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        onClose();
      }
    }
    // Use a timeout so the initial right-click doesn't immediately close the menu
    const timer = setTimeout(() => {
      document.addEventListener("mousedown", handleMouseDown);
    }, 0);
    return () => {
      clearTimeout(timer);
      document.removeEventListener("mousedown", handleMouseDown);
    };
  }, [onClose]);

  const handleAction = useCallback(
    (action: () => void) => {
      action();
      onClose();
    },
    [onClose],
  );

  const menuContent = (
    <div
      ref={menuRef}
      role="menu"
      aria-label={isBulk ? `Context menu for ${targetCards.length} cards` : "Card context menu"}
      className={clsx(
        "fixed z-[9999] min-w-[200px] py-1",
        "bg-gx-surface border border-gx-border rounded-md shadow-lg",
        "text-xs text-gx-text",
      )}
      style={{ left: adjustedPosition.x, top: adjustedPosition.y }}
    >
      {isBulk && (
        <div className="px-3 py-1.5 text-[10px] text-gx-muted border-b border-gx-border mb-1">
          {targetCards.length} cards selected
        </div>
      )}

      {!isWorkflowManaged && (
        <>
          {/* Set Priority — submenu trigger */}
          <SubmenuTrigger
            label="Set Priority"
            icon={<ChevronsUp size={14} className="text-gx-muted" />}
            shortcut=""
            isOpen={activeSubmenu === "priority"}
            onHover={() => setActiveSubmenu("priority")}
          >
            <Submenu position={submenuPosition}>
              {PRIORITIES.map((p) => (
                <MenuItem
                  key={p.value}
                  icon={<PriorityIcon priority={p.value} />}
                  label={p.label}
                  shortcut={p.shortcut}
                  onClick={() => handleAction(() => onSetPriority(p.value))}
                />
              ))}
            </Submenu>
          </SubmenuTrigger>

          {/* Assign To — submenu trigger */}
          <SubmenuTrigger
            label="Assign To"
            icon={<Users size={14} className="text-gx-muted" />}
            shortcut="A"
            isOpen={activeSubmenu === "assign"}
            onHover={() => setActiveSubmenu("assign")}
          >
            <Submenu position={submenuPosition}>
              {allAssignees.length > 0 ? (
                allAssignees.map((assignee) => (
                  <MenuItem
                    key={assignee}
                    label={assignee}
                    onClick={() => handleAction(() => onAssign(assignee))}
                  />
                ))
              ) : (
                <div className="px-3 py-1.5 text-gx-muted italic">No assignees</div>
              )}
            </Submenu>
          </SubmenuTrigger>

          {/* Move to Column — submenu trigger */}
          <SubmenuTrigger
            label="Move to Column"
            icon={<Columns size={14} className="text-gx-muted" />}
            shortcut="M"
            isOpen={activeSubmenu === "move"}
            onHover={() => setActiveSubmenu("move")}
          >
            <Submenu position={submenuPosition}>
              {columns.map((col) => (
                <MenuItem
                  key={col.id}
                  label={col.name}
                  onClick={() => handleAction(() => onMoveToColumn(col.id))}
                />
              ))}
            </Submenu>
          </SubmenuTrigger>

          <MenuDivider />
        </>
      )}

      {/* Copy Card ID */}
      <MenuItem
        icon={<Copy size={14} className="text-gx-muted" />}
        label={isBulk ? "Copy Card IDs" : "Copy Card ID"}
        shortcut="⌘C"
        onClick={() => handleAction(onCopyId)}
        onHover={() => setActiveSubmenu(null)}
      />

      {!isWorkflowManaged && (
        <MenuItem
          icon={<Bot size={14} className="text-gx-muted" />}
          label="Run Agent"
          shortcut="R"
          onClick={() => handleAction(onRunAgent)}
          onHover={() => setActiveSubmenu(null)}
        />
      )}

      {!isWorkflowManaged && onRunWorkflow && agentWorkflows.length > 0 && (
        <SubmenuTrigger
          label="Run Workflow"
          icon={<Workflow size={14} className="text-gx-muted" />}
          isOpen={activeSubmenu === "workflow"}
          onHover={() => setActiveSubmenu("workflow")}
        >
          <Submenu position={submenuPosition}>
            {agentWorkflows.map((workflow) => (
              <MenuItem
                key={workflow.id}
                label={workflow.name}
                onClick={() => handleAction(() => onRunWorkflow(workflow.id))}
              />
            ))}
          </Submenu>
        </SubmenuTrigger>
      )}

      {!isWorkflowManaged && (
        <>
          <MenuDivider />

          {/* Delete */}
          <MenuItem
            icon={<Trash2 size={14} className="text-gx-error" />}
            label={isBulk ? `Delete ${targetCards.length} Cards` : "Delete"}
            shortcut="D"
            onClick={() => handleAction(onDelete)}
            onHover={() => setActiveSubmenu(null)}
            danger
          />
        </>
      )}
    </div>
  );

  return createPortal(menuContent, document.body);
}

function isActiveSymphonyRuntime(card: KanbanCardInfo): boolean {
  const status = card.symphonyRuntime?.status;
  return status === "running" || status === "retrying" || status === "blocked" || status === "claimed";
}

// ── Internal sub-components ──────────────────────────────────────────

function MenuItem({
  icon,
  label,
  shortcut,
  onClick,
  onHover,
  danger,
}: {
  icon?: React.ReactNode;
  label: string;
  shortcut?: string;
  onClick: () => void;
  onHover?: () => void;
  danger?: boolean;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      className={clsx(
        "flex w-full items-center gap-2 px-3 py-1.5 text-left",
        "transition-colors duration-75",
        danger
          ? "hover:bg-gx-error/10 text-gx-error"
          : "hover:bg-gx-surface-hover text-gx-text",
      )}
      onClick={onClick}
      onMouseEnter={onHover}
    >
      {icon && <span className="shrink-0 w-4 flex items-center justify-center">{icon}</span>}
      <span className="flex-1 truncate">{label}</span>
      {shortcut && (
        <span className="shrink-0 text-[10px] text-gx-muted ml-4 font-mono">{shortcut}</span>
      )}
    </button>
  );
}

function SubmenuTrigger({
  label,
  icon,
  shortcut,
  isOpen,
  onHover,
  children,
}: {
  label: string;
  icon: React.ReactNode;
  shortcut?: string;
  isOpen: boolean;
  onHover: () => void;
  children: React.ReactNode;
}) {
  return (
    <div className="relative" onMouseEnter={onHover}>
      <button
        type="button"
        role="menuitem"
        aria-haspopup="true"
        aria-expanded={isOpen}
        className={clsx(
          "flex w-full items-center gap-2 px-3 py-1.5 text-left",
          "transition-colors duration-75 hover:bg-gx-surface-hover text-gx-text",
          isOpen && "bg-gx-surface-hover",
        )}
      >
        <span className="shrink-0 w-4 flex items-center justify-center">{icon}</span>
        <span className="flex-1 truncate">{label}</span>
        {shortcut && (
          <span className="shrink-0 text-[10px] text-gx-muted ml-4 font-mono">{shortcut}</span>
        )}
        <ChevronRight size={12} className="shrink-0 text-gx-muted" />
      </button>
      {isOpen && children}
    </div>
  );
}

function Submenu({
  position,
  children,
}: {
  position: "right" | "left";
  children: React.ReactNode;
}) {
  return (
    <div
      role="menu"
      className={clsx(
        "absolute top-0 z-[10000] min-w-[160px] py-1",
        "bg-gx-surface border border-gx-border rounded-md shadow-lg",
        position === "right" ? "left-full ml-0.5" : "right-full mr-0.5",
      )}
    >
      {children}
    </div>
  );
}

function MenuDivider() {
  return <div className="my-1 border-t border-gx-border" />;
}
