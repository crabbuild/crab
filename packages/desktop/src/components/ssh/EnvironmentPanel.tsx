import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
/**
 * Environment Panel — manages remote environment variables and PATH entries
 * with add/modify/delete, persist toggle, and inline validation.
 */

import clsx from "clsx";
import {
  AlertCircle,
  ChevronDown,
  ChevronUp,
  Plus,
  Save,
  ToggleLeft,
  ToggleRight,
  Trash2,
  Variable,
} from "lucide-react";
import { useCallback, useMemo, useState } from "react";

// ── Types ────────────────────────────────────────────────────────

interface EnvVariable {
  name: string;
  value: string;
}

interface EnvironmentPanelProps {
  variables: EnvVariable[];
  pathEntries: string[];
  onSetVariable: (name: string, value: string, persist: boolean) => void;
  onDeleteVariable: (name: string) => void;
  onUpdatePath: (entries: string[]) => void;
}

// ── Validation ───────────────────────────────────────────────────

const ENV_NAME_REGEX = /^[A-Za-z_][A-Za-z0-9_]{0,254}$/;

function validateEnvName(name: string): string | null {
  if (!name) return "Name is required";
  if (name.length > 255) return "Name must be 255 characters or fewer";
  if (/^\d/.test(name)) return "Name cannot start with a digit";
  if (!ENV_NAME_REGEX.test(name)) return "Name must contain only alphanumeric characters and underscores";
  return null;
}

// ── Add/Edit Variable Form ───────────────────────────────────────

function VariableForm({
  initial,
  onSave,
  onCancel,
}: {
  initial?: EnvVariable;
  onSave: (name: string, value: string, persist: boolean) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(initial?.name ?? "");
  const [value, setValue] = useState(initial?.value ?? "");
  const [persist, setPersist] = useState(false);
  const [touched, setTouched] = useState(false);

  const nameError = useMemo(() => (touched ? validateEnvName(name) : null), [name, touched]);

  const handleSubmit = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      const error = validateEnvName(name);
      if (error) {
        setTouched(true);
        return;
      }
      onSave(name, value, persist);
    },
    [name, value, persist, onSave],
  );

  return (
    <form
      onSubmit={handleSubmit}
      className="border border-border border-border rounded-md p-3 space-y-2"
    >
      <div>
        <label htmlFor="env-name" className="text-xs text-muted-foreground text-muted-foreground">
          Variable Name
        </label>
        <Input
          id="env-name"
          type="text"
          value={name}
          onChange={(e) => { setName(e.target.value); setTouched(true); }}
          placeholder="MY_VARIABLE"
          disabled={!!initial}
          className={clsx(
            "w-full px-2 py-1 text-sm border rounded bg-background bg-card",
            nameError
              ? "border-destructive border-destructive"
              : "border-border border-border",
            initial && "opacity-60 cursor-not-allowed",
          )}
          required
        />
        {nameError && (
          <p className="flex items-center gap-1 mt-0.5 text-[10px] text-destructive" role="alert">
            <AlertCircle className="w-3 h-3" />
            {nameError}
          </p>
        )}
      </div>
      <div>
        <label htmlFor="env-value" className="text-xs text-muted-foreground text-muted-foreground">
          Value
        </label>
        <Input
          id="env-value"
          type="text"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder="value"
          className="w-full px-2 py-1 text-sm border border-border border-border rounded bg-background bg-card"
        />
      </div>
      <div className="flex items-center gap-2">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={() => setPersist(!persist)}
          className="flex items-center gap-1 text-xs text-muted-foreground text-muted-foreground"
          aria-label={persist ? "Disable persistence" : "Enable persistence"}
        >
          {persist ? (
            <ToggleRight className="w-4 h-4 text-info" />
          ) : (
            <ToggleLeft className="w-4 h-4 text-muted-foreground" />
          )}
          Persist to shell config
        </Button>
      </div>
      <div className="flex justify-end gap-2 pt-1">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onCancel}
          className="px-3 py-1 text-xs text-muted-foreground text-muted-foreground hover:bg-card hover:bg-card rounded"
        >
          Cancel
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="submit"
          className="px-3 py-1 text-xs bg-info text-info-foreground rounded hover:bg-info"
        >
          <span className="flex items-center gap-1">
            <Save className="w-3 h-3" />
            {initial ? "Update" : "Add"}
          </span>
        </Button>
      </div>
    </form>
  );
}

// ── PATH Entry ───────────────────────────────────────────────────

function PathEntry({
  entry,
  index,
  total,
  onMoveUp,
  onMoveDown,
  onRemove,
}: {
  entry: string;
  index: number;
  total: number;
  onMoveUp: () => void;
  onMoveDown: () => void;
  onRemove: () => void;
}) {
  return (
    <li className="flex items-center justify-between px-2 py-1 rounded hover:bg-background hover:bg-card group">
      <span className="text-xs text-foreground text-muted-foreground truncate flex-1" title={entry}>
        {entry}
      </span>
      <div className="flex items-center gap-0.5 shrink-0 opacity-0 group-hover:opacity-100 transition-opacity">
        <Button
          variant="ghost"
          size="compact"
          onClick={onMoveUp}
          disabled={index === 0}
          className="p-0.5 text-muted-foreground hover:text-info hover:text-info rounded disabled:opacity-30 disabled:cursor-not-allowed"
          aria-label="Move up"
          title="Move up"
        >
          <ChevronUp className="w-3 h-3" />
        </Button>
        <Button
          variant="ghost"
          size="compact"
          onClick={onMoveDown}
          disabled={index === total - 1}
          className="p-0.5 text-muted-foreground hover:text-info hover:text-info rounded disabled:opacity-30 disabled:cursor-not-allowed"
          aria-label="Move down"
          title="Move down"
        >
          <ChevronDown className="w-3 h-3" />
        </Button>
        <Button
          variant="ghost"
          size="compact"
          onClick={onRemove}
          className="p-0.5 text-muted-foreground hover:text-destructive hover:text-destructive rounded"
          aria-label="Remove path entry"
          title="Remove"
        >
          <Trash2 className="w-3 h-3" />
        </Button>
      </div>
    </li>
  );
}

// ── Main Component ───────────────────────────────────────────────

export function EnvironmentPanel({
  variables,
  pathEntries,
  onSetVariable,
  onDeleteVariable,
  onUpdatePath,
}: EnvironmentPanelProps) {
  const [showAddForm, setShowAddForm] = useState(false);
  const [editingVar, setEditingVar] = useState<EnvVariable | null>(null);
  const [newPathEntry, setNewPathEntry] = useState("");
  const [showAddPath, setShowAddPath] = useState(false);

  const handleSaveVariable = useCallback(
    (name: string, value: string, persist: boolean) => {
      onSetVariable(name, value, persist);
      setShowAddForm(false);
      setEditingVar(null);
    },
    [onSetVariable],
  );

  const handleMovePathUp = useCallback(
    (index: number) => {
      if (index === 0) return;
      const updated = [...pathEntries];
      [updated[index - 1], updated[index]] = [updated[index], updated[index - 1]];
      onUpdatePath(updated);
    },
    [pathEntries, onUpdatePath],
  );

  const handleMovePathDown = useCallback(
    (index: number) => {
      if (index >= pathEntries.length - 1) return;
      const updated = [...pathEntries];
      [updated[index], updated[index + 1]] = [updated[index + 1], updated[index]];
      onUpdatePath(updated);
    },
    [pathEntries, onUpdatePath],
  );

  const handleRemovePath = useCallback(
    (index: number) => {
      const updated = pathEntries.filter((_, i) => i !== index);
      onUpdatePath(updated);
    },
    [pathEntries, onUpdatePath],
  );

  const handleAddPath = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      if (!newPathEntry.trim()) return;
      onUpdatePath([...pathEntries, newPathEntry.trim()]);
      setNewPathEntry("");
      setShowAddPath(false);
    },
    [newPathEntry, pathEntries, onUpdatePath],
  );

  return (
    <div className="flex flex-col h-full overflow-hidden" role="region" aria-label="Environment">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-border border-border">
        <h3 className="text-xs font-semibold text-muted-foreground text-muted-foreground flex items-center gap-1.5">
          <Variable className="w-4 h-4" />
          Environment
        </h3>
      </div>

      <div className="flex-1 overflow-y-auto px-3 py-2 space-y-4">
        {/* Environment Variables Section */}
        <section>
          <div className="flex items-center justify-between mb-2">
            <h4 className="text-xs font-medium text-muted-foreground text-muted-foreground">
              Variables ({variables.length})
            </h4>
            {!showAddForm && !editingVar && (
              <Button
                variant="ghost"
                size="compact"
                onClick={() => setShowAddForm(true)}
                className="flex items-center gap-1 px-2 py-0.5 text-[10px] text-info text-info hover:bg-info/10 hover:bg-info/20 rounded"
                aria-label="Add variable"
              >
                <Plus className="w-3 h-3" />
                Add
              </Button>
            )}
          </div>

          {showAddForm && (
            <VariableForm
              onSave={handleSaveVariable}
              onCancel={() => setShowAddForm(false)}
            />
          )}

          {editingVar && (
            <VariableForm
              initial={editingVar}
              onSave={handleSaveVariable}
              onCancel={() => setEditingVar(null)}
            />
          )}

          {variables.length === 0 && !showAddForm ? (
            <p className="text-xs text-muted-foreground text-muted-foreground italic">
              No environment variables loaded.
            </p>
          ) : (
            <ul className="space-y-0.5" aria-label="Environment variables">
              {variables.map((v) => (
                <li
                  key={v.name}
                  className="flex items-center justify-between px-2 py-1 rounded hover:bg-background hover:bg-card group"
                >
                  <div className="min-w-0 flex-1">
                    <span className="text-xs font-mono font-medium text-foreground text-muted-foreground">
                      {v.name}
                    </span>
                    <span className="text-xs text-muted-foreground text-muted-foreground mx-1">
                      =
                    </span>
                    <span className="text-xs text-muted-foreground text-muted-foreground truncate">
                      {v.value}
                    </span>
                  </div>
                  <div className="flex items-center gap-1 shrink-0 opacity-0 group-hover:opacity-100 transition-opacity">
                    <Button
                      variant="ghost"
                      size="compact"
                      onClick={() => setEditingVar(v)}
                      className="px-1.5 py-0.5 text-[10px] text-info text-info hover:bg-info/10 hover:bg-info/20 rounded"
                      aria-label={`Edit ${v.name}`}
                    >
                      Edit
                    </Button>
                    <Button
                      variant="ghost"
                      size="compact"
                      onClick={() => onDeleteVariable(v.name)}
                      className="p-0.5 text-destructive hover:bg-destructive/10 hover:bg-destructive/20 rounded"
                      aria-label={`Delete ${v.name}`}
                    >
                      <Trash2 className="w-3 h-3" />
                    </Button>
                  </div>
                </li>
              ))}
            </ul>
          )}
        </section>

        {/* PATH Section */}
        <section>
          <div className="flex items-center justify-between mb-2">
            <h4 className="text-xs font-medium text-muted-foreground text-muted-foreground">
              PATH ({pathEntries.length})
            </h4>
            {!showAddPath && (
              <Button
                variant="ghost"
                size="compact"
                onClick={() => setShowAddPath(true)}
                className="flex items-center gap-1 px-2 py-0.5 text-[10px] text-info text-info hover:bg-info/10 hover:bg-info/20 rounded"
                aria-label="Add PATH entry"
              >
                <Plus className="w-3 h-3" />
                Add
              </Button>
            )}
          </div>

          {showAddPath && (
            <form onSubmit={handleAddPath} className="flex items-center gap-2 mb-2">
              <Input
                type="text"
                value={newPathEntry}
                onChange={(e) => setNewPathEntry(e.target.value)}
                placeholder="/usr/local/bin"
                className="flex-1 px-2 py-1 text-xs border border-border border-border rounded bg-background bg-card"
                autoFocus
              />
              <Button
                variant="ghost"
                size="compact"
                type="submit"
                className="px-2 py-1 text-xs bg-info text-info-foreground rounded hover:bg-info"
              >
                Add
              </Button>
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={() => { setShowAddPath(false); setNewPathEntry(""); }}
                className="px-2 py-1 text-xs text-muted-foreground text-muted-foreground hover:bg-card hover:bg-card rounded"
              >
                Cancel
              </Button>
            </form>
          )}

          {pathEntries.length === 0 ? (
            <p className="text-xs text-muted-foreground text-muted-foreground italic">
              No PATH entries loaded.
            </p>
          ) : (
            <ol className="space-y-0.5" aria-label="PATH entries">
              {pathEntries.map((entry, index) => (
                <PathEntry
                  key={`${index}-${entry}`}
                  entry={entry}
                  index={index}
                  total={pathEntries.length}
                  onMoveUp={() => handleMovePathUp(index)}
                  onMoveDown={() => handleMovePathDown(index)}
                  onRemove={() => handleRemovePath(index)}
                />
              ))}
            </ol>
          )}
        </section>
      </div>
    </div>
  );
}

// ── Exports for testing ──────────────────────────────────────────

export { ENV_NAME_REGEX, validateEnvName };
