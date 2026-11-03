import { useCallback, useEffect, useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  GripVertical,
  Minus,
  Play,
  Plus,
  Settings,
  Trash2,
  X,
} from "lucide-react";
import type { KanbanAutomationRule, KanbanCardInfo } from "../../ipc";

// ── Constants ───────────────────────────────────────────────────────

const TRIGGER_TYPES = [
  { value: "card_moved", label: "Card Moved" },
  { value: "issue_assigned", label: "Issue Assigned" },
  { value: "label_added", label: "Label Added" },
  { value: "pr_review_requested", label: "PR Review Requested" },
  { value: "ci_failed", label: "CI Failed" },
  { value: "pr_comment", label: "PR Comment" },
  { value: "schedule", label: "Schedule" },
  { value: "manual", label: "Manual" },
] as const;

const CONDITION_TYPES = [
  { value: "label_matches", label: "Label Matches", paramKey: "pattern", paramLabel: "Glob pattern" },
  { value: "complexity_below", label: "Complexity Below", paramKey: "threshold", paramLabel: "Threshold (1–10)" },
  { value: "has_description", label: "Has Description", paramKey: null, paramLabel: null },
  { value: "no_linked_pr", label: "No Linked PR", paramKey: null, paramLabel: null },
  { value: "assignee_is_me", label: "Assignee Is Me", paramKey: null, paramLabel: null },
] as const;

const ACTION_TYPES = [
  { value: "spawn_agent", label: "Spawn Agent" },
  { value: "create_pr", label: "Create PR" },
  { value: "run_tests", label: "Run Tests" },
  { value: "request_review", label: "Request Review" },
  { value: "transition_issue", label: "Transition Issue" },
  { value: "add_comment", label: "Add Comment" },
  { value: "merge_pr", label: "Merge PR" },
  { value: "iterate_agent", label: "Iterate Agent" },
] as const;

// ── Interfaces ──────────────────────────────────────────────────────

interface RulesPanelProps {
  open: boolean;
  onClose: () => void;
  boardId: string;
  cards: KanbanCardInfo[];
}

interface RuleFormState {
  id: string;
  name: string;
  enabled: boolean;
  triggerType: string;
  triggerParams: Record<string, unknown>;
  conditions: Array<{ type: string; params: Record<string, unknown> }>;
  actions: Array<{ type: string; params: Record<string, unknown> }>;
  continueOnError: boolean;
}

// ── Helpers ─────────────────────────────────────────────────────────

function emptyFormState(): RuleFormState {
  return {
    id: "",
    name: "",
    enabled: true,
    triggerType: "card_moved",
    triggerParams: {},
    conditions: [],
    actions: [],
    continueOnError: false,
  };
}

function ruleToFormState(rule: KanbanAutomationRule): RuleFormState {
  return {
    id: rule.id,
    name: rule.name,
    enabled: rule.enabled,
    triggerType: rule.trigger.type,
    triggerParams: rule.trigger.params ?? {},
    conditions: rule.conditions.map((c) => ({ type: c.type, params: { ...c.params } })),
    actions: rule.actions.map((a) => ({ type: a.type, params: { ...a.params } })),
    continueOnError: rule.continueOnError,
  };
}

function formStateToRule(form: RuleFormState): KanbanAutomationRule {
  return {
    id: form.id,
    name: form.name,
    enabled: form.enabled,
    trigger: { type: form.triggerType, params: form.triggerParams },
    conditions: form.conditions,
    actions: form.actions,
    continueOnError: form.continueOnError,
  };
}

// ── Component ───────────────────────────────────────────────────────

/**
 * Slide-over panel for managing automation rules. Supports creating,
 * editing, enabling/disabling, deleting rules, and manually firing
 * a rule against a selected card.
 */
export function RulesPanel({ open, onClose, boardId, cards }: RulesPanelProps) {
  const [rules, setRules] = useState<KanbanAutomationRule[]>([]);
  const [editingRule, setEditingRule] = useState<RuleFormState | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState<string | null>(null);
  const [runNowRuleId, setRunNowRuleId] = useState<string | null>(null);
  const [runNowCardId, setRunNowCardId] = useState<string>("");

  // Load rules when panel opens
  useEffect(() => {
    if (!open || !boardId) return;
    let cancelled = false;
    async function load() {
      try {
        const loaded = await window.crab.kanban.getRules(boardId);
        if (!cancelled) setRules(loaded);
      } catch {
        // Rules may not be available yet
      }
    }
    load();
    return () => { cancelled = true; };
  }, [open, boardId]);

  // ── Rule CRUD ───────────────────────────────────────────────────

  const handleToggleEnabled = useCallback(async (rule: KanbanAutomationRule) => {
    const updated = { ...rule, enabled: !rule.enabled };
    try {
      const saved = await window.crab.kanban.saveRule(boardId, updated);
      if (saved) {
        setRules((prev) => prev.map((r) => (r.id === saved.id ? saved : r)));
      }
    } catch {
      // Revert on failure — no-op since we didn't update local state yet
    }
  }, [boardId]);

  const handleSave = useCallback(async () => {
    if (!editingRule) return;
    const rule = formStateToRule(editingRule);
    try {
      const saved = await window.crab.kanban.saveRule(boardId, rule);
      if (saved) {
        setRules((prev) => {
          const idx = prev.findIndex((r) => r.id === saved.id);
          if (idx >= 0) {
            const next = [...prev];
            next[idx] = saved;
            return next;
          }
          return [...prev, saved];
        });
      }
      setEditingRule(null);
    } catch {
      // Save failed — keep form open
    }
  }, [editingRule, boardId]);

  const handleDelete = useCallback(async (ruleId: string) => {
    try {
      await window.crab.kanban.deleteRule(boardId, ruleId);
      setRules((prev) => prev.filter((r) => r.id !== ruleId));
      setDeleteConfirm(null);
    } catch {
      // Delete failed
    }
  }, [boardId]);

  const handleRunNow = useCallback(async () => {
    if (!runNowRuleId || !runNowCardId) return;
    try {
      await window.crab.kanban.fireRule(boardId, runNowRuleId, runNowCardId);
    } catch {
      // Fire failed
    }
    setRunNowRuleId(null);
    setRunNowCardId("");
  }, [boardId, runNowRuleId, runNowCardId]);

  // ── Keyboard: Escape closes panel ──────────────────────────────

  useEffect(() => {
    if (!open) return;
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        if (editingRule) {
          setEditingRule(null);
        } else {
          onClose();
        }
      }
    }
    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [open, editingRule, onClose]);

  if (!open) return null;

  return (
    <>
      {/* Backdrop */}
      <div
        className="fixed inset-0 z-40 bg-black/40"
        onClick={onClose}
        aria-hidden="true"
      />

      {/* Slide-over panel */}
      <div
        className="fixed inset-y-0 right-0 z-50 flex w-[420px] max-w-full flex-col border-l border-gx-border bg-gx-surface shadow-xl"
        role="dialog"
        aria-label="Automation Rules"
      >
        {/* Header */}
        <div className="flex items-center gap-2 border-b border-gx-border px-4 py-3">
          <Settings size={14} className="text-gx-muted" />
          <h2 className="text-sm font-medium text-gx-text">Automation Rules</h2>
          <div className="flex-1" />
          <button
            type="button"
            className="btn-icon"
            onClick={onClose}
            aria-label="Close rules panel"
          >
            <X size={14} />
          </button>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto">
          {editingRule ? (
            <RuleForm
              form={editingRule}
              onChange={setEditingRule}
              onSave={handleSave}
              onCancel={() => setEditingRule(null)}
            />
          ) : (
            <div className="p-3 space-y-2">
              {/* Add rule button */}
              <button
                type="button"
                className="btn btn-primary flex w-full items-center justify-center gap-1.5 text-xs"
                onClick={() => setEditingRule(emptyFormState())}
              >
                <Plus size={12} />
                New Rule
              </button>

              {/* Rule list */}
              {rules.length === 0 && (
                <p className="py-4 text-center text-xs text-gx-muted">
                  No automation rules configured.
                </p>
              )}
              {rules.map((rule) => (
                <RuleListItem
                  key={rule.id}
                  rule={rule}
                  onToggle={() => handleToggleEnabled(rule)}
                  onEdit={() => setEditingRule(ruleToFormState(rule))}
                  onDelete={() => setDeleteConfirm(rule.id)}
                  onRunNow={() => {
                    setRunNowRuleId(rule.id);
                    setRunNowCardId(cards[0]?.id ?? "");
                  }}
                />
              ))}
            </div>
          )}
        </div>
      </div>

      {/* Delete confirmation dialog */}
      {deleteConfirm && (
        <div
          className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50"
          onClick={() => setDeleteConfirm(null)}
        >
          <div
            className="w-72 rounded-lg border border-gx-border bg-gx-surface p-4 shadow-2xl"
            onClick={(e) => e.stopPropagation()}
          >
            <h3 className="text-sm font-medium text-gx-text">Delete rule?</h3>
            <p className="mt-1 text-xs text-gx-muted">
              This action cannot be undone. The rule will be permanently removed.
            </p>
            <div className="mt-3 flex justify-end gap-2">
              <button
                type="button"
                className="btn text-xs"
                onClick={() => setDeleteConfirm(null)}
              >
                Cancel
              </button>
              <button
                type="button"
                className="btn btn-danger text-xs"
                onClick={() => handleDelete(deleteConfirm)}
              >
                Delete
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Run Now dialog — select card */}
      {runNowRuleId && (
        <div
          className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50"
          onClick={() => setRunNowRuleId(null)}
        >
          <div
            className="w-80 rounded-lg border border-gx-border bg-gx-surface p-4 shadow-2xl"
            onClick={(e) => e.stopPropagation()}
          >
            <h3 className="text-sm font-medium text-gx-text">Run Rule Now</h3>
            <p className="mt-1 text-xs text-gx-muted">
              Select a card to fire this rule against.
            </p>
            <select
              className="input-field mt-2 w-full text-xs"
              value={runNowCardId}
              onChange={(e) => setRunNowCardId(e.target.value)}
            >
              {cards.length === 0 && <option value="">No cards available</option>}
              {cards.map((card) => (
                <option key={card.id} value={card.id}>
                  {card.externalId} — {card.title.slice(0, 60)}
                </option>
              ))}
            </select>
            <div className="mt-3 flex justify-end gap-2">
              <button
                type="button"
                className="btn text-xs"
                onClick={() => setRunNowRuleId(null)}
              >
                Cancel
              </button>
              <button
                type="button"
                className="btn btn-primary text-xs"
                onClick={handleRunNow}
                disabled={!runNowCardId}
              >
                <Play size={12} className="mr-1" />
                Run
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}

// ── Rule List Item ──────────────────────────────────────────────────

interface RuleListItemProps {
  rule: KanbanAutomationRule;
  onToggle: () => void;
  onEdit: () => void;
  onDelete: () => void;
  onRunNow: () => void;
}

function RuleListItem({ rule, onToggle, onEdit, onDelete, onRunNow }: RuleListItemProps) {
  const triggerLabel = TRIGGER_TYPES.find((t) => t.value === rule.trigger.type)?.label ?? rule.trigger.type;

  return (
    <div
      className={clsx(
        "rounded border border-gx-border p-2.5 transition",
        rule.enabled ? "bg-gx-bg-sub" : "bg-gx-bg-sub/50 opacity-60",
      )}
    >
      <div className="flex items-center gap-2">
        {/* Enable/disable toggle */}
        <button
          type="button"
          className={clsx(
            "relative h-4 w-7 shrink-0 rounded-full transition",
            rule.enabled ? "bg-gx-accent" : "bg-gx-muted/40",
          )}
          onClick={onToggle}
          aria-label={rule.enabled ? "Disable rule" : "Enable rule"}
          title={rule.enabled ? "Disable" : "Enable"}
        >
          <span
            className={clsx(
              "absolute top-0.5 h-3 w-3 rounded-full bg-white transition-transform",
              rule.enabled ? "left-3.5" : "left-0.5",
            )}
          />
        </button>

        {/* Rule name — clickable to edit */}
        <button
          type="button"
          className="flex-1 text-left text-xs font-medium text-gx-text truncate hover:text-gx-accent transition"
          onClick={onEdit}
          title="Edit rule"
        >
          {rule.name || "Untitled Rule"}
        </button>

        {/* Run Now */}
        <button
          type="button"
          className="btn-icon"
          onClick={onRunNow}
          title="Run now"
          aria-label="Run rule now"
        >
          <Play size={12} />
        </button>

        {/* Delete */}
        <button
          type="button"
          className="btn-icon text-gx-muted hover:text-gx-error"
          onClick={onDelete}
          title="Delete rule"
          aria-label="Delete rule"
        >
          <Trash2 size={12} />
        </button>
      </div>

      {/* Trigger badge */}
      <div className="mt-1.5 flex items-center gap-1.5">
        <span className="chip text-[10px] px-1.5 py-0">{triggerLabel}</span>
        {rule.conditions.length > 0 && (
          <span className="text-[10px] text-gx-muted">
            {rule.conditions.length} condition{rule.conditions.length > 1 ? "s" : ""}
          </span>
        )}
        <span className="text-[10px] text-gx-muted">
          → {rule.actions.length} action{rule.actions.length > 1 ? "s" : ""}
        </span>
      </div>
    </div>
  );
}

// ── Rule Form ───────────────────────────────────────────────────────

interface RuleFormProps {
  form: RuleFormState;
  onChange: (form: RuleFormState) => void;
  onSave: () => void;
  onCancel: () => void;
}

function RuleForm({ form, onChange, onSave, onCancel }: RuleFormProps) {
  const update = (patch: Partial<RuleFormState>) => onChange({ ...form, ...patch });

  return (
    <div className="p-3 space-y-3">
      <h3 className="text-xs font-medium text-gx-text">
        {form.id ? "Edit Rule" : "New Rule"}
      </h3>

      {/* Name */}
      <div>
        <label className="block text-[10px] text-gx-muted mb-0.5">Rule Name</label>
        <input
          type="text"
          className="input-field w-full text-xs"
          placeholder="e.g., Auto-start on Ready"
          value={form.name}
          onChange={(e) => update({ name: e.target.value })}
        />
      </div>

      {/* Trigger */}
      <div>
        <label className="block text-[10px] text-gx-muted mb-0.5">Trigger</label>
        <div className="relative">
          <select
            className="input-field w-full text-xs appearance-none pr-6"
            value={form.triggerType}
            onChange={(e) => update({ triggerType: e.target.value, triggerParams: {} })}
          >
            {TRIGGER_TYPES.map((t) => (
              <option key={t.value} value={t.value}>{t.label}</option>
            ))}
          </select>
          <ChevronDown size={12} className="absolute right-2 top-1/2 -translate-y-1/2 text-gx-muted pointer-events-none" />
        </div>
      </div>

      {/* Conditions */}
      <div>
        <div className="flex items-center justify-between mb-1">
          <label className="text-[10px] text-gx-muted">Conditions</label>
          <button
            type="button"
            className="btn-icon"
            onClick={() =>
              update({
                conditions: [...form.conditions, { type: "label_matches", params: {} }],
              })
            }
            title="Add condition"
            aria-label="Add condition"
          >
            <Plus size={12} />
          </button>
        </div>
        {form.conditions.length === 0 && (
          <p className="text-[10px] text-gx-muted italic">No conditions — rule always fires.</p>
        )}
        {form.conditions.map((cond, idx) => (
          <ConditionRow
            key={idx}
            condition={cond}
            onChange={(updated) => {
              const next = [...form.conditions];
              next[idx] = updated;
              update({ conditions: next });
            }}
            onRemove={() => {
              update({ conditions: form.conditions.filter((_, i) => i !== idx) });
            }}
          />
        ))}
      </div>

      {/* Actions */}
      <div>
        <div className="flex items-center justify-between mb-1">
          <label className="text-[10px] text-gx-muted">Actions</label>
          <button
            type="button"
            className="btn-icon"
            onClick={() =>
              update({
                actions: [...form.actions, { type: "spawn_agent", params: {} }],
              })
            }
            title="Add action"
            aria-label="Add action"
          >
            <Plus size={12} />
          </button>
        </div>
        {form.actions.length === 0 && (
          <p className="text-[10px] text-gx-muted italic">No actions configured.</p>
        )}
        {form.actions.map((action, idx) => (
          <ActionRow
            key={idx}
            action={action}
            index={idx}
            total={form.actions.length}
            onChange={(updated) => {
              const next = [...form.actions];
              next[idx] = updated;
              update({ actions: next });
            }}
            onRemove={() => {
              update({ actions: form.actions.filter((_, i) => i !== idx) });
            }}
            onMoveUp={() => {
              if (idx === 0) return;
              const next = [...form.actions];
              [next[idx - 1], next[idx]] = [next[idx], next[idx - 1]];
              update({ actions: next });
            }}
            onMoveDown={() => {
              if (idx === form.actions.length - 1) return;
              const next = [...form.actions];
              [next[idx], next[idx + 1]] = [next[idx + 1], next[idx]];
              update({ actions: next });
            }}
          />
        ))}
      </div>

      {/* Continue on error toggle */}
      <div className="flex items-center gap-2">
        <button
          type="button"
          className={clsx(
            "relative h-4 w-7 shrink-0 rounded-full transition",
            form.continueOnError ? "bg-gx-accent" : "bg-gx-muted/40",
          )}
          onClick={() => update({ continueOnError: !form.continueOnError })}
          aria-label="Continue on error"
        >
          <span
            className={clsx(
              "absolute top-0.5 h-3 w-3 rounded-full bg-white transition-transform",
              form.continueOnError ? "left-3.5" : "left-0.5",
            )}
          />
        </button>
        <span className="text-xs text-gx-text">Continue on error</span>
      </div>

      {/* Save / Cancel */}
      <div className="flex justify-end gap-2 pt-2 border-t border-gx-border">
        <button type="button" className="btn text-xs" onClick={onCancel}>
          Cancel
        </button>
        <button
          type="button"
          className="btn btn-primary text-xs"
          onClick={onSave}
          disabled={!form.name.trim()}
        >
          {form.id ? "Save" : "Create"}
        </button>
      </div>
    </div>
  );
}

// ── Condition Row ───────────────────────────────────────────────────

interface ConditionRowProps {
  condition: { type: string; params: Record<string, unknown> };
  onChange: (cond: { type: string; params: Record<string, unknown> }) => void;
  onRemove: () => void;
}

function ConditionRow({ condition, onChange, onRemove }: ConditionRowProps) {
  const condDef = CONDITION_TYPES.find((c) => c.value === condition.type);

  return (
    <div className="flex items-center gap-1.5 mb-1.5">
      <select
        className="input-field flex-1 text-xs"
        value={condition.type}
        onChange={(e) => onChange({ type: e.target.value, params: {} })}
      >
        {CONDITION_TYPES.map((c) => (
          <option key={c.value} value={c.value}>{c.label}</option>
        ))}
      </select>
      {condDef?.paramKey && (
        <input
          type={condDef.value === "complexity_below" ? "number" : "text"}
          className="input-field w-24 text-xs"
          placeholder={condDef.paramLabel ?? ""}
          value={(condition.params[condDef.paramKey] as string) ?? ""}
          onChange={(e) =>
            onChange({
              ...condition,
              params: { ...condition.params, [condDef.paramKey!]: e.target.value },
            })
          }
        />
      )}
      <button
        type="button"
        className="btn-icon text-gx-muted hover:text-gx-error"
        onClick={onRemove}
        title="Remove condition"
        aria-label="Remove condition"
      >
        <Minus size={12} />
      </button>
    </div>
  );
}

// ── Action Row ──────────────────────────────────────────────────────

interface ActionRowProps {
  action: { type: string; params: Record<string, unknown> };
  index: number;
  total: number;
  onChange: (action: { type: string; params: Record<string, unknown> }) => void;
  onRemove: () => void;
  onMoveUp: () => void;
  onMoveDown: () => void;
}

function ActionRow({ action, index, total, onChange, onRemove, onMoveUp, onMoveDown }: ActionRowProps) {
  return (
    <div className="flex items-center gap-1.5 mb-1.5">
      {/* Reorder grip */}
      <div className="flex flex-col">
        <button
          type="button"
          className="btn-icon h-3 w-3"
          onClick={onMoveUp}
          disabled={index === 0}
          title="Move up"
          aria-label="Move action up"
        >
          <GripVertical size={10} className={index === 0 ? "opacity-30" : ""} />
        </button>
        <button
          type="button"
          className="btn-icon h-3 w-3"
          onClick={onMoveDown}
          disabled={index === total - 1}
          title="Move down"
          aria-label="Move action down"
        >
          <GripVertical size={10} className={index === total - 1 ? "opacity-30" : ""} />
        </button>
      </div>

      <select
        className="input-field flex-1 text-xs"
        value={action.type}
        onChange={(e) => onChange({ type: e.target.value, params: {} })}
      >
        {ACTION_TYPES.map((a) => (
          <option key={a.value} value={a.value}>{a.label}</option>
        ))}
      </select>

      <button
        type="button"
        className="btn-icon text-gx-muted hover:text-gx-error"
        onClick={onRemove}
        title="Remove action"
        aria-label="Remove action"
      >
        <Minus size={12} />
      </button>
    </div>
  );
}
