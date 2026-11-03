import { useCallback, useEffect, useId, useRef, useState } from "react";
import clsx from "clsx";
import {
  ArrowDown,
  ArrowUp,
  Plus,
  Settings,
  Trash2,
  X,
} from "lucide-react";
import { TrackerConnect } from "./TrackerConnect";
import type {
  BoardFieldDefinition,
  BoardFieldType,
  KanbanAutonomyLevel,
  KanbanBoardConfig,
  KanbanColumnInfo,
} from "../../ipc";

interface BoardSettingsProps {
  board: KanbanBoardConfig;
  onClose: () => void;
  onSave: (config: Partial<KanbanBoardConfig>) => void;
}

const MAX_COLUMNS = 20;
const MAX_COLUMN_NAME_LENGTH = 64;
const MIN_POLL_INTERVAL = 10;
const MAX_POLL_INTERVAL = 3600;
const FIELD_TYPES: BoardFieldType[] = ["text", "number", "select", "multi-select", "user", "date", "checkbox", "url"];

/**
 * Board settings dialog. Provides column editor (add/remove/reorder,
 * status mapping), polling interval, autonomy level, and max iterations
 * configuration. Persists changes within 1s via the onSave callback.
 */
export function BoardSettings({ board, onClose, onSave }: BoardSettingsProps) {
  const headingId = useId();
  const closeRef = useRef<HTMLButtonElement | null>(null);
  const previousActive = useRef<HTMLElement | null>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Tab state
  const [activeTab, setActiveTab] = useState<"columns" | "fields" | "runs" | "tracker" | "symphony">("columns");

  // Local state mirrors board config for editing
  const [columns, setColumns] = useState<KanbanColumnInfo[]>(board.columns);
  const [pollInterval, setPollInterval] = useState(
    Math.round(board.pollIntervalMs / 1000),
  );
  const [autonomyLevel, setAutonomyLevel] = useState<KanbanAutonomyLevel>(
    board.autonomyLevel,
  );
  const [maxIterations, setMaxIterations] = useState(board.maxIterations);
  const [fields, setFields] = useState<BoardFieldDefinition[]>(board.fields ?? []);
  const [runDefaults, setRunDefaults] = useState({
    agentType: board.runDefaults?.agentType ?? "codex",
    model: board.runDefaults?.model ?? "",
    reasoningLevel: board.runDefaults?.reasoningLevel ?? "",
    baseRef: board.runDefaults?.baseRef ?? "HEAD",
    branchName: board.runDefaults?.branchName ?? "",
    mode: board.runDefaults?.mode ?? "instant",
    openChat: board.runDefaults?.openChat ?? true,
  });
  const [symphonyConfig, setSymphonyConfig] = useState({
    enabled: board.symphony?.enabled ?? false,
    workflowPath: board.symphony?.workflowPath ?? "",
    autostart: board.symphony?.autostart ?? false,
    projectToKanban: board.symphony?.projectToKanban ?? true,
  });

  // Focus management
  useEffect(() => {
    previousActive.current = (document.activeElement as HTMLElement | null) ?? null;
    closeRef.current?.focus();
    return () => {
      previousActive.current?.focus?.();
    };
  }, []);

  // Esc closes
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [onClose]);

  // Debounced persist — saves within 1s of any modification
  const persistChanges = useCallback(
    (updates: Partial<KanbanBoardConfig>) => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
      debounceRef.current = setTimeout(() => {
        onSave(updates);
      }, 800);
    },
    [onSave],
  );

  // Immediate persist for tracker connection changes
  const persistChangesImmediate = useCallback(
    (updates: Partial<KanbanBoardConfig>) => {
      onSave(updates);
    },
    [onSave],
  );

  // Clean up debounce on unmount
  useEffect(() => {
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, []);

  // ── Column Editor ─────────────────────────────────────────────

  const handleAddColumn = useCallback(() => {
    if (columns.length >= MAX_COLUMNS) return;
    const newCol: KanbanColumnInfo = {
      id: crypto.randomUUID(),
      name: "",
      trackerStatusMapping: [],
    };
    const updated = [...columns, newCol];
    setColumns(updated);
    persistChanges({ columns: updated });
  }, [columns, persistChanges]);

  const handleRemoveColumn = useCallback(
    (colId: string) => {
      const updated = columns.filter((c) => c.id !== colId);
      setColumns(updated);
      persistChanges({ columns: updated });
    },
    [columns, persistChanges],
  );

  const handleColumnNameChange = useCallback(
    (colId: string, name: string) => {
      const trimmed = name.slice(0, MAX_COLUMN_NAME_LENGTH);
      const updated = columns.map((c) =>
        c.id === colId ? { ...c, name: trimmed } : c,
      );
      setColumns(updated);
      persistChanges({ columns: updated });
    },
    [columns, persistChanges],
  );

  const handleStatusMappingChange = useCallback(
    (colId: string, mapping: string) => {
      const statuses = mapping
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean);
      const updated = columns.map((c) =>
        c.id === colId ? { ...c, trackerStatusMapping: statuses } : c,
      );
      setColumns(updated);
      persistChanges({ columns: updated });
    },
    [columns, persistChanges],
  );

  const handleMoveColumn = useCallback(
    (colId: string, direction: "up" | "down") => {
      const idx = columns.findIndex((c) => c.id === colId);
      if (idx === -1) return;
      if (direction === "up" && idx === 0) return;
      if (direction === "down" && idx === columns.length - 1) return;

      const updated = [...columns];
      const swapIdx = direction === "up" ? idx - 1 : idx + 1;
      [updated[idx], updated[swapIdx]] = [updated[swapIdx], updated[idx]];
      setColumns(updated);
      persistChanges({ columns: updated });
    },
    [columns, persistChanges],
  );

  // ── Polling Interval ──────────────────────────────────────────

  const handlePollIntervalChange = useCallback(
    (value: number) => {
      const clamped = Math.max(MIN_POLL_INTERVAL, Math.min(MAX_POLL_INTERVAL, value));
      setPollInterval(clamped);
      persistChanges({ pollIntervalMs: clamped * 1000 });
    },
    [persistChanges],
  );

  // ── Autonomy Level ────────────────────────────────────────────

  const handleAutonomyChange = useCallback(
    (level: KanbanAutonomyLevel) => {
      setAutonomyLevel(level);
      persistChanges({ autonomyLevel: level });
    },
    [persistChanges],
  );

  // ── Max Iterations ────────────────────────────────────────────

  const handleMaxIterationsChange = useCallback(
    (value: number) => {
      const clamped = Math.max(1, Math.min(20, value));
      setMaxIterations(clamped);
      persistChanges({ maxIterations: clamped });
    },
    [persistChanges],
  );

  const handleFieldsChange = useCallback(
    (next: BoardFieldDefinition[]) => {
      setFields(next);
      persistChanges({ fields: next });
    },
    [persistChanges],
  );

  const handleRunDefaultsChange = useCallback(
    (updates: Partial<typeof runDefaults>) => {
      setRunDefaults((prev) => {
        const next = { ...prev, ...updates };
        persistChanges({
          runDefaults: {
            agentType: next.agentType,
            model: next.model.trim() || undefined,
            reasoningLevel: next.reasoningLevel.trim() || undefined,
            baseRef: next.baseRef.trim() || "HEAD",
            branchName: next.branchName.trim() || undefined,
            mode: next.mode as "plan" | "instant",
            openChat: next.openChat,
            worktreeMode: "new",
          },
        });
        return next;
      });
    },
    [persistChanges],
  );

  const handleSymphonyChange = useCallback(
    (updates: Partial<typeof symphonyConfig>) => {
      setSymphonyConfig((prev) => {
        const next = { ...prev, ...updates };
        persistChangesImmediate({
          symphony: {
            enabled: next.enabled,
            workflowPath: next.workflowPath.trim() || undefined,
            autostart: next.autostart,
            projectToKanban: next.projectToKanban,
          },
        });
        return next;
      });
    },
    [persistChangesImmediate],
  );

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={headingId}
        className="flex max-h-[80vh] w-[520px] max-w-[calc(100vw-2rem)] flex-col rounded-lg border border-gx-border bg-gx-surface shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-gx-border px-4 py-3">
          <Settings size={14} className="text-gx-muted" />
          <h2 id={headingId} className="text-sm font-medium text-gx-text">
            Board Settings
          </h2>
          <div className="flex-1" />
          <button
            ref={closeRef}
            type="button"
            className="btn-icon"
            onClick={onClose}
            title="Close"
          >
            <X size={14} />
          </button>
        </div>

        {/* Tabs */}
        <div className="flex border-b border-gx-border">
          <button
            type="button"
            className={clsx(
              "px-4 py-2 text-xs transition",
              activeTab === "columns"
                ? "border-b-2 border-gx-accent text-gx-text font-medium"
                : "text-gx-muted hover:text-gx-text",
            )}
            onClick={() => setActiveTab("columns")}
          >
            Board
          </button>
          <button
            type="button"
            className={clsx(
              "px-4 py-2 text-xs transition",
              activeTab === "fields"
                ? "border-b-2 border-gx-accent text-gx-text font-medium"
                : "text-gx-muted hover:text-gx-text",
            )}
            onClick={() => setActiveTab("fields")}
          >
            Fields
          </button>
          <button
            type="button"
            className={clsx(
              "px-4 py-2 text-xs transition",
              activeTab === "runs"
                ? "border-b-2 border-gx-accent text-gx-text font-medium"
                : "text-gx-muted hover:text-gx-text",
            )}
            onClick={() => setActiveTab("runs")}
          >
            Runs
          </button>
          <button
            type="button"
            className={clsx(
              "px-4 py-2 text-xs transition",
              activeTab === "tracker"
                ? "border-b-2 border-gx-accent text-gx-text font-medium"
                : "text-gx-muted hover:text-gx-text",
            )}
            onClick={() => setActiveTab("tracker")}
          >
            Tracker
          </button>
          <button
            type="button"
            className={clsx(
              "px-4 py-2 text-xs transition",
              activeTab === "symphony"
                ? "border-b-2 border-gx-accent text-gx-text font-medium"
                : "text-gx-muted hover:text-gx-text",
            )}
            onClick={() => setActiveTab("symphony")}
          >
            Symphony
          </button>
        </div>

        {/* Scrollable content */}
        <div className="flex-1 overflow-y-auto p-4 space-y-5">
          {activeTab === "tracker" ? (
            <TrackerConnect board={board} onSave={persistChangesImmediate} />
          ) : activeTab === "fields" ? (
            <section>
              <div className="mb-2 flex items-center justify-between">
                <h3 className="text-xs font-medium text-gx-text">
                  Fields ({fields.length})
                </h3>
                <button
                  type="button"
                  className="btn flex items-center gap-1 text-[11px]"
                  onClick={() => {
                    handleFieldsChange([
                      ...fields,
                      {
                        id: crypto.randomUUID(),
                        name: "New field",
                        type: "text",
                        taskScoped: true,
                        boardScoped: false,
                        createdAt: Date.now(),
                        updatedAt: Date.now(),
                      },
                    ]);
                  }}
                >
                  <Plus size={12} />
                  Add
                </button>
              </div>
              <div className="space-y-1.5">
                {fields.map((field) => (
                  <div key={field.id} className="flex items-center gap-1.5 rounded border border-gx-border bg-gx-bg-sub p-1.5">
                    <input
                      className="input-field min-w-0 flex-1"
                      value={field.name}
                      onChange={(event) => {
                        handleFieldsChange(fields.map((item) => item.id === field.id
                          ? { ...item, name: event.target.value, updatedAt: Date.now() }
                          : item));
                      }}
                    />
                    <select
                      className="input-field w-32"
                      value={field.type}
                      onChange={(event) => {
                        handleFieldsChange(fields.map((item) => item.id === field.id
                          ? { ...item, type: event.target.value as BoardFieldType, updatedAt: Date.now() }
                          : item));
                      }}
                    >
                      {FIELD_TYPES.map((type) => <option key={type} value={type}>{type}</option>)}
                    </select>
                    <button
                      type="button"
                      className="btn-icon text-gx-muted hover:text-gx-error"
                      onClick={() => handleFieldsChange(fields.filter((item) => item.id !== field.id))}
                      title="Remove field"
                    >
                      <Trash2 size={12} />
                    </button>
                  </div>
                ))}
                {fields.length === 0 && (
                  <p className="py-2 text-center text-[11px] text-gx-muted">
                    No custom fields. Add fields for project-specific task attributes.
                  </p>
                )}
              </div>
            </section>
          ) : activeTab === "runs" ? (
            <div className="space-y-4">
              <section>
                <h3 className="mb-2 text-xs font-medium text-gx-text">Agent defaults</h3>
                <div className="grid grid-cols-2 gap-3">
                  <label>
                    <span className="mb-1 block text-[10px] text-gx-muted">Agent</span>
                    <select
                      className="input-field w-full"
                      value={runDefaults.agentType}
                      onChange={(event) => handleRunDefaultsChange({ agentType: event.target.value })}
                    >
                      <option value="codex">Codex</option>
                      <option value="claude-code">Claude Code</option>
                      <option value="custom">Custom</option>
                    </select>
                  </label>
                  <label>
                    <span className="mb-1 block text-[10px] text-gx-muted">Mode</span>
                    <select
                      className="input-field w-full"
                      value={runDefaults.mode}
                      onChange={(event) => handleRunDefaultsChange({ mode: event.target.value as "plan" | "instant" })}
                    >
                      <option value="instant">Implement</option>
                      <option value="plan">Plan first</option>
                    </select>
                  </label>
                  <label>
                    <span className="mb-1 block text-[10px] text-gx-muted">Base ref</span>
                    <input
                      className="input-field w-full"
                      value={runDefaults.baseRef}
                      onChange={(event) => handleRunDefaultsChange({ baseRef: event.target.value })}
                    />
                  </label>
                  <label>
                    <span className="mb-1 block text-[10px] text-gx-muted">Branch prefix/name</span>
                    <input
                      className="input-field w-full"
                      value={runDefaults.branchName}
                      onChange={(event) => handleRunDefaultsChange({ branchName: event.target.value })}
                      placeholder="agent generated"
                    />
                  </label>
                  <label>
                    <span className="mb-1 block text-[10px] text-gx-muted">Model</span>
                    <input
                      className="input-field w-full"
                      value={runDefaults.model}
                      onChange={(event) => handleRunDefaultsChange({ model: event.target.value })}
                      placeholder="Agent default"
                    />
                  </label>
                  <label>
                    <span className="mb-1 block text-[10px] text-gx-muted">Intelligence</span>
                    <input
                      className="input-field w-full"
                      value={runDefaults.reasoningLevel}
                      onChange={(event) => handleRunDefaultsChange({ reasoningLevel: event.target.value })}
                      placeholder="default, high, xhigh"
                    />
                  </label>
                </div>
              </section>
              <label className="flex items-center gap-2 text-xs text-gx-text">
                <input
                  type="checkbox"
                  className="accent-gx-accent"
                  checked={runDefaults.openChat}
                  onChange={(event) => handleRunDefaultsChange({ openChat: event.target.checked })}
                />
                Open chat after starting a task
              </label>
            </div>
          ) : activeTab === "symphony" ? (
            <div className="space-y-4">
              <section>
                <h3 className="text-xs font-medium text-gx-text mb-2">
                  Runtime
                </h3>
                <div className="space-y-2">
                  <label className="flex items-start gap-2 rounded border border-gx-border p-2">
                    <input
                      type="checkbox"
                      className="mt-0.5 accent-gx-accent"
                      checked={symphonyConfig.enabled}
                      onChange={(e) => handleSymphonyChange({ enabled: e.target.checked })}
                    />
                    <div className="min-w-0">
                      <span className="text-xs font-medium text-gx-text">Enable Symphony mode</span>
                      <p className="text-[10px] text-gx-muted">
                        Linear issues are projected into Kanban and executed by the Symphony scheduler.
                      </p>
                    </div>
                  </label>
                  <label className="flex items-start gap-2 rounded border border-gx-border p-2">
                    <input
                      type="checkbox"
                      className="mt-0.5 accent-gx-accent"
                      checked={symphonyConfig.autostart}
                      onChange={(e) => handleSymphonyChange({ autostart: e.target.checked })}
                      disabled={!symphonyConfig.enabled}
                    />
                    <div className="min-w-0">
                      <span className="text-xs font-medium text-gx-text">Autostart runtime</span>
                      <p className="text-[10px] text-gx-muted">
                        Starts only when this board has Symphony enabled and the app loads.
                      </p>
                    </div>
                  </label>
                  <label className="flex items-start gap-2 rounded border border-gx-border p-2">
                    <input
                      type="checkbox"
                      className="mt-0.5 accent-gx-accent"
                      checked={symphonyConfig.projectToKanban}
                      onChange={(e) => handleSymphonyChange({ projectToKanban: e.target.checked })}
                      disabled={!symphonyConfig.enabled}
                    />
                    <div className="min-w-0">
                      <span className="text-xs font-medium text-gx-text">Project issues into Kanban</span>
                      <p className="text-[10px] text-gx-muted">
                        Keeps Linear issue cards visible with runtime state and Codex telemetry.
                      </p>
                    </div>
                  </label>
                </div>
              </section>

              <section>
                <h3 className="text-xs font-medium text-gx-text mb-2">
                  Workflow
                </h3>
                <label className="text-[10px] text-gx-muted block mb-0.5">
                  Workflow path
                </label>
                <input
                  type="text"
                  className="input-field w-full"
                  placeholder="WORKFLOW.md"
                  value={symphonyConfig.workflowPath}
                  onChange={(e) => handleSymphonyChange({ workflowPath: e.target.value })}
                  disabled={!symphonyConfig.enabled}
                />
                <p className="mt-1 text-[10px] text-gx-muted">
                  Empty uses &lt;repo&gt;/WORKFLOW.md. Explicit paths must resolve inside the board repository.
                </p>
              </section>
            </div>
          ) : (
          <>
          {/* ── Columns Section ─────────────────────────────── */}
          <section>
            <div className="flex items-center justify-between mb-2">
              <h3 className="text-xs font-medium text-gx-text">
                Columns ({columns.length}/{MAX_COLUMNS})
              </h3>
              <button
                type="button"
                className="btn flex items-center gap-1 text-[11px]"
                onClick={handleAddColumn}
                disabled={columns.length >= MAX_COLUMNS}
              >
                <Plus size={12} />
                Add
              </button>
            </div>
            <div className="space-y-1.5">
              {columns.map((col, idx) => (
                <div
                  key={col.id}
                  className="flex items-center gap-1.5 rounded border border-gx-border bg-gx-bg-sub p-1.5"
                >
                  {/* Reorder buttons */}
                  <div className="flex flex-col gap-0.5">
                    <button
                      type="button"
                      className="btn-icon h-4 w-4"
                      onClick={() => handleMoveColumn(col.id, "up")}
                      disabled={idx === 0}
                      title="Move up"
                    >
                      <ArrowUp size={10} />
                    </button>
                    <button
                      type="button"
                      className="btn-icon h-4 w-4"
                      onClick={() => handleMoveColumn(col.id, "down")}
                      disabled={idx === columns.length - 1}
                      title="Move down"
                    >
                      <ArrowDown size={10} />
                    </button>
                  </div>

                  {/* Column name */}
                  <input
                    type="text"
                    className="input-field flex-1 min-w-0"
                    placeholder="Column name"
                    value={col.name}
                    maxLength={MAX_COLUMN_NAME_LENGTH}
                    onChange={(e) => handleColumnNameChange(col.id, e.target.value)}
                  />

                  {/* Status mapping */}
                  <input
                    type="text"
                    className="input-field w-32"
                    placeholder="Status mapping"
                    title="Comma-separated tracker statuses (e.g., Open, To Do)"
                    value={col.trackerStatusMapping.join(", ")}
                    onChange={(e) => handleStatusMappingChange(col.id, e.target.value)}
                  />

                  {/* Remove */}
                  <button
                    type="button"
                    className="btn-icon text-gx-muted hover:text-gx-error"
                    onClick={() => handleRemoveColumn(col.id)}
                    title="Remove column"
                  >
                    <Trash2 size={12} />
                  </button>
                </div>
              ))}
              {columns.length === 0 && (
                <p className="text-[11px] text-gx-muted py-2 text-center">
                  No columns configured. Add at least one column.
                </p>
              )}
            </div>
          </section>

          {/* ── Polling Interval ────────────────────────────── */}
          <section>
            <h3 className="text-xs font-medium text-gx-text mb-2">
              Polling Interval
            </h3>
            <div className="flex items-center gap-3">
              <input
                type="range"
                min={MIN_POLL_INTERVAL}
                max={MAX_POLL_INTERVAL}
                step={10}
                value={pollInterval}
                onChange={(e) => handlePollIntervalChange(Number(e.target.value))}
                className="flex-1 accent-gx-accent"
              />
              <div className="flex items-center gap-1">
                <input
                  type="number"
                  className="input-field w-16 text-center"
                  min={MIN_POLL_INTERVAL}
                  max={MAX_POLL_INTERVAL}
                  value={pollInterval}
                  onChange={(e) => handlePollIntervalChange(Number(e.target.value))}
                />
                <span className="text-[11px] text-gx-muted">sec</span>
              </div>
            </div>
            <p className="mt-1 text-[10px] text-gx-muted">
              How often to poll the tracker for updates ({MIN_POLL_INTERVAL}s – {MAX_POLL_INTERVAL}s)
            </p>
          </section>

          {/* ── Autonomy Level ──────────────────────────────── */}
          <section>
            <h3 className="text-xs font-medium text-gx-text mb-2">
              Autonomy Level
            </h3>
            <div className="flex flex-col gap-1.5">
              {(
                [
                  {
                    value: "conservative" as const,
                    label: "Conservative",
                    desc: "Agent works, user approves every step",
                  },
                  {
                    value: "moderate" as const,
                    label: "Moderate",
                    desc: "Agent works → tests → PR → review → user approves merge",
                  },
                  {
                    value: "aggressive" as const,
                    label: "Aggressive",
                    desc: "Full automation including merge on approval + CI pass",
                  },
                ] as const
              ).map((opt) => (
                <label
                  key={opt.value}
                  className={clsx(
                    "flex items-start gap-2 rounded border p-2 cursor-pointer transition",
                    autonomyLevel === opt.value
                      ? "border-gx-accent/60 bg-gx-accent/5"
                      : "border-gx-border hover:border-gx-accent/30",
                  )}
                >
                  <input
                    type="radio"
                    name="autonomy-level"
                    value={opt.value}
                    checked={autonomyLevel === opt.value}
                    onChange={() => handleAutonomyChange(opt.value)}
                    className="mt-0.5 accent-gx-accent"
                  />
                  <div className="min-w-0">
                    <span className="text-xs font-medium text-gx-text">
                      {opt.label}
                    </span>
                    <p className="text-[10px] text-gx-muted leading-tight">
                      {opt.desc}
                    </p>
                  </div>
                </label>
              ))}
            </div>
          </section>

          {/* ── Max Iterations ──────────────────────────────── */}
          <section>
            <h3 className="text-xs font-medium text-gx-text mb-2">
              Max Iterations
            </h3>
            <div className="flex items-center gap-2">
              <input
                type="number"
                className="input-field w-20"
                min={1}
                max={20}
                value={maxIterations}
                onChange={(e) => handleMaxIterationsChange(Number(e.target.value))}
              />
              <span className="text-[11px] text-gx-muted">
                retries before blocking (default: 3)
              </span>
            </div>
            <p className="mt-1 text-[10px] text-gx-muted">
              Maximum agent re-runs per card on CI failure or review feedback.
            </p>
          </section>
          </>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end border-t border-gx-border px-4 py-3">
          <p className="flex-1 text-[10px] text-gx-muted">
            Changes are saved automatically.
          </p>
          <button
            type="button"
            className="btn text-xs"
            onClick={onClose}
          >
            Done
          </button>
        </div>
      </div>
    </div>
  );
}
