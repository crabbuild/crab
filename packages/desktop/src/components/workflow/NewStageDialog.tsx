import { useState } from "react";
import { X } from "lucide-react";
import { WorkflowStage } from "../../lib/workflow-types";

interface Props {
  existingNames: string[];
  onAdd: (stage: WorkflowStage) => void;
  onClose: () => void;
}

export function NewStageDialog({ existingNames, onAdd, onClose }: Props) {
  const [name, setName] = useState("");
  const [cmd, setCmd] = useState("");
  const [error, setError] = useState<string | null>(null);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);

    // Validate name.
    const trimmedName = name.trim();
    if (!trimmedName) {
      setError("Stage name is required.");
      return;
    }
    if (!/^[a-zA-Z_][a-zA-Z0-9_-]*$/.test(trimmedName)) {
      setError("Name must start with a letter or underscore, and contain only letters, digits, _ or -.");
      return;
    }
    if (trimmedName.length > 64) {
      setError("Name must be 64 characters or fewer.");
      return;
    }
    if (existingNames.includes(trimmedName)) {
      setError(`Stage "${trimmedName}" already exists.`);
      return;
    }

    const trimmedCmd = cmd.trim();
    if (!trimmedCmd) {
      setError("Command is required.");
      return;
    }

    const stage: WorkflowStage = {
      name: trimmedName,
      cmd: { kind: "shell", value: trimmedCmd },
      deps: [],
      outs: [],
      params: [],
      env: "inherit",
    };

    onAdd(stage);
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div className="w-[400px] rounded-lg border border-gx-border bg-gx-surface shadow-xl">
        <div className="flex items-center justify-between border-b border-gx-border px-4 py-3">
          <h2 className="text-sm font-medium text-gx-text">New Stage</h2>
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
            onClick={onClose}
          >
            <X size={16} />
          </button>
        </div>

        <form onSubmit={handleSubmit} className="p-4">
          <div className="space-y-3">
            <div>
              <label className="mb-1 block text-xs text-gx-muted">Stage Name</label>
              <input
                type="text"
                className="input-field w-full text-sm"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="e.g. train, preprocess, evaluate"
                autoFocus
              />
            </div>

            <div>
              <label className="mb-1 block text-xs text-gx-muted">Command</label>
              <textarea
                className="input-field w-full resize-y font-mono text-xs"
                rows={3}
                value={cmd}
                onChange={(e) => setCmd(e.target.value)}
                placeholder="python train.py --epochs 100"
              />
            </div>

            {error && (
              <div className="rounded border border-gx-error/40 bg-gx-error/10 px-3 py-2 text-xs text-gx-error">
                {error}
              </div>
            )}
          </div>

          <div className="mt-4 flex justify-end gap-2">
            <button type="button" className="btn text-xs" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="btn btn-primary text-xs">
              Add Stage
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
