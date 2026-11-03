import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";
import {
  ArrowRightLeft,
  ChevronDown,
  Trash2,
  UserCircle,
  X,
  ChevronsUp,
  ChevronUp,
  Minus,
  Circle,
} from "lucide-react";
import type { KanbanCardPriority, KanbanColumnInfo } from "../../ipc";

export interface BulkActionBarProps {
  selectedCount: number;
  columns: KanbanColumnInfo[];
  onMoveToColumn: (columnId: string) => void;
  onSetPriority: (priority: KanbanCardPriority) => void;
  onAssign: (assignee: string) => void;
  onDelete: () => void;
  onClearSelection: () => void;
}

const PRIORITIES: { value: KanbanCardPriority; label: string; icon: React.ReactNode }[] = [
  { value: "critical", label: "Critical", icon: <ChevronsUp size={12} className="text-gx-error" /> },
  { value: "high", label: "High", icon: <ChevronUp size={12} className="text-gx-warn" /> },
  { value: "medium", label: "Medium", icon: <Minus size={12} className="text-yellow-400" /> },
  { value: "low", label: "Low", icon: <ChevronDown size={12} className="text-gx-info" /> },
  { value: "none", label: "None", icon: <Circle size={10} className="text-gx-muted" /> },
];

/**
 * Dropdown menu for bulk actions. Renders a button that toggles a
 * floating list of options above it.
 */
function BulkDropdown({
  label,
  icon,
  children,
}: {
  label: string;
  icon: React.ReactNode;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function handleClickOutside(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    function handleEscape(e: KeyboardEvent) {
      if (e.key === "Escape") setOpen(false);
    }
    document.addEventListener("mousedown", handleClickOutside);
    document.addEventListener("keydown", handleEscape);
    return () => {
      document.removeEventListener("mousedown", handleClickOutside);
      document.removeEventListener("keydown", handleEscape);
    };
  }, [open]);

  return (
    <div ref={containerRef} className="relative">
      <button
        type="button"
        className="btn text-xs gap-1 px-2 py-1"
        onClick={() => setOpen((prev) => !prev)}
        aria-expanded={open}
        aria-haspopup="listbox"
      >
        {icon}
        <span>{label}</span>
        <ChevronDown size={10} className="text-gx-muted" />
      </button>
      {open && (
        <div
          className="absolute bottom-full left-0 mb-1 min-w-[160px] rounded-md border border-gx-border bg-gx-surface shadow-lg z-50 py-1"
          role="listbox"
        >
          {children}
        </div>
      )}
    </div>
  );
}

/**
 * Fixed-bottom floating action bar shown when one or more cards are selected.
 * Provides bulk operations: move to column, set priority, assign, delete,
 * and clear selection.
 */
export function BulkActionBar({
  selectedCount,
  columns,
  onMoveToColumn,
  onSetPriority,
  onAssign,
  onDelete,
  onClearSelection,
}: BulkActionBarProps) {
  const [assignInput, setAssignInput] = useState("");
  const [showAssignInput, setShowAssignInput] = useState(false);
  const assignInputRef = useRef<HTMLInputElement>(null);

  const handleAssignSubmit = useCallback(() => {
    const trimmed = assignInput.trim();
    if (trimmed.length > 0) {
      onAssign(trimmed);
      setAssignInput("");
      setShowAssignInput(false);
    }
  }, [assignInput, onAssign]);

  useEffect(() => {
    if (showAssignInput && assignInputRef.current) {
      assignInputRef.current.focus();
    }
  }, [showAssignInput]);

  if (selectedCount <= 0) return null;

  return (
    <div
      className={clsx(
        "fixed bottom-4 left-1/2 -translate-x-1/2 z-50",
        "flex items-center gap-2 px-4 py-2",
        "rounded-lg border border-gx-border bg-gx-surface shadow-xl",
        "animate-in fade-in slide-in-from-bottom-2 duration-150",
      )}
      role="toolbar"
      aria-label="Bulk actions"
    >
      {/* Selection count */}
      <span className="text-xs font-medium text-gx-text whitespace-nowrap">
        {selectedCount} selected
      </span>

      <div className="w-px h-4 bg-gx-border" />

      {/* Move to column dropdown */}
      <BulkDropdown
        label="Move"
        icon={<ArrowRightLeft size={12} className="text-gx-muted" />}
      >
        {columns.map((col) => (
          <button
            key={col.id}
            type="button"
            role="option"
            className="w-full text-left px-3 py-1.5 text-xs text-gx-text hover:bg-gx-surface-hover transition-colors"
            onClick={() => onMoveToColumn(col.id)}
          >
            {col.name}
          </button>
        ))}
      </BulkDropdown>

      {/* Set priority dropdown */}
      <BulkDropdown
        label="Priority"
        icon={<ChevronsUp size={12} className="text-gx-muted" />}
      >
        {PRIORITIES.map((p) => (
          <button
            key={p.value}
            type="button"
            role="option"
            className="w-full text-left px-3 py-1.5 text-xs text-gx-text hover:bg-gx-surface-hover transition-colors flex items-center gap-2"
            onClick={() => onSetPriority(p.value)}
          >
            {p.icon}
            <span>{p.label}</span>
          </button>
        ))}
      </BulkDropdown>

      {/* Assign dropdown/input */}
      <div className="relative">
        {showAssignInput ? (
          <div className="flex items-center gap-1">
            <input
              ref={assignInputRef}
              type="text"
              className="input-field text-xs px-2 py-1 w-28"
              placeholder="Assignee…"
              value={assignInput}
              onChange={(e) => setAssignInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  handleAssignSubmit();
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  setShowAssignInput(false);
                  setAssignInput("");
                }
              }}
            />
            <button
              type="button"
              className="btn-icon h-5 w-5 text-gx-muted hover:text-gx-text"
              onClick={() => {
                setShowAssignInput(false);
                setAssignInput("");
              }}
              aria-label="Cancel assign"
            >
              <X size={10} />
            </button>
          </div>
        ) : (
          <button
            type="button"
            className="btn text-xs gap-1 px-2 py-1"
            onClick={() => setShowAssignInput(true)}
          >
            <UserCircle size={12} className="text-gx-muted" />
            <span>Assign</span>
          </button>
        )}
      </div>

      <div className="w-px h-4 bg-gx-border" />

      {/* Delete */}
      <button
        type="button"
        className="btn-danger text-xs px-2 py-1 rounded-md"
        onClick={onDelete}
        aria-label={`Delete ${selectedCount} selected cards`}
      >
        <Trash2 size={12} />
        <span>Delete</span>
      </button>

      <div className="w-px h-4 bg-gx-border" />

      {/* Clear selection */}
      <button
        type="button"
        className="btn text-xs gap-1 px-2 py-1"
        onClick={onClearSelection}
        aria-label="Clear selection"
      >
        <X size={12} className="text-gx-muted" />
        <span>Clear</span>
      </button>
    </div>
  );
}
