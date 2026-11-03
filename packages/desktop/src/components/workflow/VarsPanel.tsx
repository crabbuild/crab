import { useCallback, useState } from "react";
import { Plus, Trash2 } from "lucide-react";

interface Props {
  vars: Record<string, string>;
  onChange: (vars: Record<string, string>) => void;
}

export function VarsPanel({ vars, onChange }: Props) {
  const entries = Object.entries(vars);

  const handleKeyChange = useCallback(
    (oldKey: string, newKey: string) => {
      if (newKey === oldKey) return;
      const updated: Record<string, string> = {};
      for (const [k, v] of Object.entries(vars)) {
        if (k === oldKey) {
          updated[newKey] = v;
        } else {
          updated[k] = v;
        }
      }
      onChange(updated);
    },
    [vars, onChange],
  );

  const handleValueChange = useCallback(
    (key: string, value: string) => {
      onChange({ ...vars, [key]: value });
    },
    [vars, onChange],
  );

  const handleAdd = useCallback(() => {
    // Generate a unique default key.
    let idx = entries.length + 1;
    let key = `var_${idx}`;
    while (key in vars) {
      idx++;
      key = `var_${idx}`;
    }
    onChange({ ...vars, [key]: "" });
  }, [vars, entries.length, onChange]);

  const handleRemove = useCallback(
    (key: string) => {
      const updated = { ...vars };
      delete updated[key];
      onChange(updated);
    },
    [vars, onChange],
  );

  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-center justify-between">
        <span className="text-xs font-medium text-gx-text">Variables</span>
        <button
          type="button"
          className="btn flex items-center gap-1 text-xs"
          onClick={handleAdd}
        >
          <Plus size={12} /> Add
        </button>
      </div>

      {entries.length === 0 && (
        <div className="py-4 text-center text-xs text-gx-muted">
          No variables declared. Click "Add" to create one.
        </div>
      )}

      <div className="space-y-1.5">
        {entries.map(([key, value]) => (
          <VarRow
            key={key}
            varKey={key}
            varValue={value}
            onKeyChange={(newKey) => handleKeyChange(key, newKey)}
            onValueChange={(newValue) => handleValueChange(key, newValue)}
            onRemove={() => handleRemove(key)}
          />
        ))}
      </div>
    </div>
  );
}

function VarRow({
  varKey,
  varValue,
  onKeyChange,
  onValueChange,
  onRemove,
}: {
  varKey: string;
  varValue: string;
  onKeyChange: (key: string) => void;
  onValueChange: (value: string) => void;
  onRemove: () => void;
}) {
  const [editingKey, setEditingKey] = useState(varKey);

  const commitKey = useCallback(() => {
    const trimmed = editingKey.trim();
    if (trimmed && trimmed !== varKey) {
      onKeyChange(trimmed);
    } else {
      setEditingKey(varKey);
    }
  }, [editingKey, varKey, onKeyChange]);

  return (
    <div className="group flex items-center gap-1.5">
      <input
        className="input-field w-28 shrink-0 font-mono text-xs"
        value={editingKey}
        onChange={(e) => setEditingKey(e.target.value)}
        onBlur={commitKey}
        onKeyDown={(e) => {
          if (e.key === "Enter") commitKey();
        }}
        placeholder="key"
      />
      <span className="text-xs text-gx-muted">=</span>
      <input
        className="input-field min-w-0 flex-1 font-mono text-xs"
        value={varValue}
        onChange={(e) => onValueChange(e.target.value)}
        placeholder="value"
      />
      <button
        type="button"
        className="rounded p-0.5 text-gx-muted opacity-0 hover:text-gx-error group-hover:opacity-100"
        onClick={onRemove}
        title="Remove variable"
      >
        <Trash2 size={12} />
      </button>
    </div>
  );
}
