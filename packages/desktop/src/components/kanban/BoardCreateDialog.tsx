import { useState } from "react";
import { Columns3, Plus, Trash2, X } from "lucide-react";
import type { BoardFieldDefinition, BoardFieldType, KanbanBoardConfig } from "../../ipc";

interface BoardCreateDialogProps {
  repoPath?: string;
  onClose: () => void;
  onCreate: (config: Partial<KanbanBoardConfig>) => void;
}

const FIELD_TYPES: BoardFieldType[] = ["text", "number", "select", "multi-select", "user", "date", "checkbox", "url"];

export function BoardCreateDialog({ repoPath, onClose, onCreate }: BoardCreateDialogProps) {
  const [name, setName] = useState("Agent Board");
  const [trackerType, setTrackerType] = useState<KanbanBoardConfig["trackerType"]>("local");
  const [fields, setFields] = useState<BoardFieldDefinition[]>([
    {
      id: crypto.randomUUID(),
      name: "Estimate",
      type: "number",
      taskScoped: true,
      boardScoped: false,
      createdAt: Date.now(),
      updatedAt: Date.now(),
    },
  ]);
  const [agentType, setAgentType] = useState("codex");
  const [model, setModel] = useState("");
  const [baseRef, setBaseRef] = useState("HEAD");

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div className="flex max-h-[86vh] w-[560px] max-w-[calc(100vw-2rem)] flex-col rounded-md border border-gx-border bg-gx-surface shadow-2xl">
        <div className="flex items-center gap-2 border-b border-gx-border px-4 py-3">
          <Columns3 size={14} className="text-gx-accent" />
          <h2 className="text-sm font-medium text-gx-text">Create board</h2>
          <div className="flex-1" />
          <button type="button" className="btn-icon" onClick={onClose} title="Close">
            <X size={14} />
          </button>
        </div>

        <div className="flex-1 space-y-5 overflow-y-auto p-4">
          <section className="grid grid-cols-2 gap-3">
            <label>
              <span className="mb-1 block text-[10px] font-medium uppercase tracking-wide text-gx-muted">
                Name
              </span>
              <input
                className="input-field w-full"
                value={name}
                onChange={(event) => setName(event.target.value)}
                autoFocus
              />
            </label>
            <label>
              <span className="mb-1 block text-[10px] font-medium uppercase tracking-wide text-gx-muted">
                Tracker
              </span>
              <select
                className="input-field w-full"
                value={trackerType}
                onChange={(event) => setTrackerType(event.target.value as KanbanBoardConfig["trackerType"])}
              >
                <option value="local">Local</option>
                <option value="github">GitHub Issues</option>
                <option value="github-projects">GitHub Projects</option>
                <option value="linear">Linear</option>
                <option value="jira">Jira</option>
              </select>
            </label>
          </section>

          <section>
            <div className="mb-2 flex items-center justify-between">
              <h3 className="text-xs font-medium text-gx-text">Task fields</h3>
              <button
                type="button"
                className="btn flex items-center gap-1 text-[11px]"
                onClick={() => {
                  setFields((current) => [
                    ...current,
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
                <div key={field.id} className="flex items-center gap-2 rounded border border-gx-border bg-gx-bg-sub p-1.5">
                  <input
                    className="input-field min-w-0 flex-1"
                    value={field.name}
                    onChange={(event) => {
                      const value = event.target.value;
                      setFields((current) => current.map((item) => item.id === field.id ? { ...item, name: value, updatedAt: Date.now() } : item));
                    }}
                  />
                  <select
                    className="input-field w-32"
                    value={field.type}
                    onChange={(event) => {
                      const value = event.target.value as BoardFieldType;
                      setFields((current) => current.map((item) => item.id === field.id ? { ...item, type: value, updatedAt: Date.now() } : item));
                    }}
                  >
                    {FIELD_TYPES.map((type) => <option key={type} value={type}>{type}</option>)}
                  </select>
                  <button
                    type="button"
                    className="btn-icon text-gx-muted hover:text-gx-error"
                    onClick={() => setFields((current) => current.filter((item) => item.id !== field.id))}
                    title="Remove field"
                  >
                    <Trash2 size={12} />
                  </button>
                </div>
              ))}
            </div>
          </section>

          <section>
            <h3 className="mb-2 text-xs font-medium text-gx-text">Run defaults</h3>
            <div className="grid grid-cols-3 gap-3">
              <label>
                <span className="mb-1 block text-[10px] font-medium uppercase tracking-wide text-gx-muted">Agent</span>
                <select className="input-field w-full" value={agentType} onChange={(event) => setAgentType(event.target.value)}>
                  <option value="codex">Codex</option>
                  <option value="claude-code">Claude Code</option>
                  <option value="custom">Custom</option>
                </select>
              </label>
              <label>
                <span className="mb-1 block text-[10px] font-medium uppercase tracking-wide text-gx-muted">Base ref</span>
                <input className="input-field w-full" value={baseRef} onChange={(event) => setBaseRef(event.target.value)} />
              </label>
              <label>
                <span className="mb-1 block text-[10px] font-medium uppercase tracking-wide text-gx-muted">Model</span>
                <input className="input-field w-full" value={model} onChange={(event) => setModel(event.target.value)} placeholder="default" />
              </label>
            </div>
          </section>
        </div>

        <div className="flex items-center justify-end gap-2 border-t border-gx-border px-4 py-3">
          <button type="button" className="btn text-xs" onClick={onClose}>Cancel</button>
          <button
            type="button"
            className="btn btn-primary text-xs"
            onClick={() => {
              const trimmedName = name.trim();
              if (!trimmedName) return;
              onCreate({
                name: trimmedName,
                repoPath: repoPath ?? "",
                trackerType,
                fields: fields.filter((field) => field.name.trim()).map((field) => ({ ...field, name: field.name.trim() })),
                runDefaults: {
                  agentType,
                  baseRef: baseRef.trim() || "HEAD",
                  model: model.trim() || undefined,
                  worktreeMode: "new",
                  mode: "instant",
                  openChat: true,
                },
              });
            }}
          >
            Create board
          </button>
        </div>
      </div>
    </div>
  );
}
