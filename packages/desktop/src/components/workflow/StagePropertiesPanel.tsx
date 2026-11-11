import { Input } from "@/components/ui/input";
import { NativeSelect } from "@/components/ui/native-select";
import { Textarea } from "@/components/ui/textarea";
import { useCallback, useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  FolderOpen,
  Play,
  Plus,
  Snowflake,
  Trash2,
  X,
} from "lucide-react";
import {
  WorkflowStage,
  CmdKind,
  StageRunState,
  StageDep,
  StageOut,
  OutKind,
  ConditionSpec,
} from "../../lib/workflow-types";
import { ConditionBuilder } from "./ConditionBuilder";
import { mainProcess } from "../../ipc";

interface Props {
  stage: WorkflowStage;
  workflowOutputs: Array<{ file: string; stage: string; path: string }>;
  runStatus: StageRunState | null;
  onUpdate: (stage: WorkflowStage) => void;
  onRename: (oldName: string, newName: string) => void;
  onDelete: () => void;
  onClose: () => void;
  onRun: () => void;
  isRunning: boolean;
}

export function StagePropertiesPanel({
  stage,
  runStatus,
  workflowOutputs,
  onUpdate,
  onRename,
  onDelete,
  onClose,
  onRun,
  isRunning,
}: Props) {
  const [expandedSections, setExpandedSections] = useState<Set<string>>(
    new Set(["command", "deps", "outs"]),
  );

  const toggleSection = (id: string) => {
    setExpandedSections((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const updateCmd = useCallback(
    (value: string) => {
      onUpdate({ ...stage, cmd: { ...stage.cmd, value } });
    },
    [stage, onUpdate],
  );

  const addDep = useCallback(async () => {
    const path = await mainProcess.dialogPrompt("Dependency path:");
    if (!path) return;
    const dep: StageDep = { kind: "path", value: path };
    onUpdate({ ...stage, deps: [...stage.deps, dep] });
  }, [stage, onUpdate]);

  const removeDep = useCallback(
    (index: number) => {
      const deps = stage.deps.filter((_, i) => i !== index);
      onUpdate({ ...stage, deps });
    },
    [stage, onUpdate],
  );

  const addOut = useCallback(async () => {
    const path = await mainProcess.dialogPrompt("Output path:");
    if (!path) return;
    const out: StageOut = { path, kind: "file" as OutKind };
    onUpdate({ ...stage, outs: [...stage.outs, out] });
  }, [stage, onUpdate]);

  const removeOut = useCallback(
    (index: number) => {
      const outs = stage.outs.filter((_, i) => i !== index);
      onUpdate({ ...stage, outs });
    },
    [stage, onUpdate],
  );

  const addParam = useCallback(async () => {
    const param = await mainProcess.dialogPrompt("Parameter (dotted path, e.g. model.lr):");
    if (!param) return;
    onUpdate({ ...stage, params: [...stage.params, param] });
  }, [stage, onUpdate]);

  const removeParam = useCallback(
    (index: number) => {
      const params = stage.params.filter((_, i) => i !== index);
      onUpdate({ ...stage, params });
    },
    [stage, onUpdate],
  );

  return (
    <aside aria-label="Stage properties" className="flex w-72 shrink-0 xl:w-80 flex-col border-l border-border bg-card">
      {/* Header */}
      <div className="flex items-center gap-2 border-b border-border px-3 py-2">
        <Input
          key={stage.name}
          aria-label="Stage name"
          type="text"
          className="min-w-0 flex-1 text-sm font-medium"
          defaultValue={stage.name}
          onBlur={(e) => {
            const newName = e.target.value.trim();
            if (newName && newName !== stage.name) {
              onRename(stage.name, newName);
            }
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          }}
        />
        <button
          type="button"
          className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
          onClick={onRun}
          disabled={isRunning}
          title="Run this stage"
        >
          <Play size={14} />
        </button>
        <button
          type="button"
          className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-destructive"
          onClick={onDelete}
          title="Delete stage"
        >
          <Trash2 size={14} />
        </button>
        <button
          type="button"
          className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
          onClick={onClose}
          title="Close"
        >
          <X size={14} />
        </button>
      </div>

      {/* Run status */}
      {runStatus && (
        <div className="border-b border-border px-3 py-2">
          <div className="flex items-center gap-2 text-xs">
            <span
              className={clsx(
                "inline-block h-2 w-2 rounded-full",
                runStatus.status === "done" && "bg-success",
                runStatus.status === "running" && "bg-info animate-pulse",
                runStatus.status === "failed" && "bg-destructive",
                runStatus.status === "cached" && "bg-thinking",
                runStatus.status === "pending" && "bg-muted-foreground",
              )}
            />
            <span className="text-muted-foreground capitalize">{runStatus.status}</span>
            {runStatus.duration_ms != null && (
              <span className="ml-auto text-muted-foreground">
                {(runStatus.duration_ms / 1000).toFixed(1)}s
              </span>
            )}
          </div>
          {runStatus.error && (
            <div className="mt-1 rounded bg-destructive/10 px-2 py-1 text-[10px] text-destructive">
              {runStatus.error}
            </div>
          )}
        </div>
      )}

      {/* Scrollable content */}
      <div className="flex-1 overflow-y-auto">
        {/* Command section */}
        {Boolean(stage.foreach || stage.matrix) && <p className="px-3 py-2 text-xs text-muted-foreground">{stage.foreach ? "Foreach" : "Matrix"} template. Changes apply to every execution. Use Edit YAML to change iterations or axes.</p>}
        <Section
          id="command"
          label="Command"
          expanded={expandedSections.has("command")}
          onToggle={toggleSection}
        >
          <div className="space-y-2">
            <div className="text-[10px] text-muted-foreground/80">
              The shell command or program this stage executes when run.
            </div>
            <NativeSelect
              className="w-full text-xs"
              value={stage.cmd.kind}
              onChange={(e) =>
                onUpdate({ ...stage, cmd: { ...stage.cmd, kind: e.target.value as CmdKind } })
              }
            >
              <option value="shell">Shell</option>
              <option value="argv">Argv</option>
              <option value="shell_list">Shell commands</option>
            </NativeSelect>
            <Textarea
              aria-label="Stage command"
              className="w-full resize-y font-mono text-xs"
              rows={3}
              value={stage.cmd.value}
              onChange={(e) => updateCmd(e.target.value)}
              placeholder={stage.cmd.kind === "shell" ? "python train.py" : '["python", "train.py"]'}
            />
            <div className="text-[10px] text-muted-foreground/60">
              {stage.cmd.kind === "shell"
                ? "Tip: Use shell syntax. Variables like $MODEL_PATH are expanded at runtime."
                : stage.cmd.kind === "argv"
                  ? "Tip: Provide a JSON array of arguments. No shell expansion occurs."
                  : "Provide a JSON array of shell commands, executed in order."}
            </div>
          </div>
        </Section>

        {/* Dependencies section */}
        <Section
          id="deps"
          label={`Dependencies (${stage.deps.length})`}
          expanded={expandedSections.has("deps")}
          onToggle={toggleSection}
        >
          <div className="space-y-1">
            <div className="mb-2 text-[10px] text-muted-foreground/80">
              Files or stage outputs this stage needs before it can run. The stage is re-executed when any dependency changes.
            </div>
            {stage.deps.map((dep, i) => (
              <div key={i} className="group flex items-center gap-1">
                <span className="rounded bg-background px-1 py-0.5 text-[10px] text-muted-foreground">
                  {dep.kind}
                </span>
                <span className="flex-1 truncate font-mono text-xs text-foreground">
                  {dep.value}
                </span>
                <button
                  type="button"
                  aria-label={`Remove dependency ${dep.value}`}
                  className="rounded p-1 opacity-0 focus:opacity-100 group-hover:opacity-100"
                  onClick={() => removeDep(i)}
                >
                  <X size={12} className="text-muted-foreground hover:text-destructive" />
                </button>
              </div>
            ))}
            {stage.deps.length === 0 && (
              <div className="rounded bg-background/50 px-2 py-1.5 text-[10px] text-muted-foreground/60 italic">
                No dependencies. This stage will always run from scratch.
              </div>
            )}
            {workflowOutputs.length > 0 && (
              <label className="block pt-2 text-xs text-muted-foreground">
                Use a workflow output
                <NativeSelect aria-label="Use a workflow output" className="mt-1 w-full text-xs" value="" onChange={event => {
                  if (!event.target.value) return;
                  const dep: StageDep = { kind: "path", value: event.target.value };
                  if (!stage.deps.some(existing => existing.kind === "path" && existing.value === dep.value)) onUpdate({ ...stage, deps: [...stage.deps, dep] });
                }}>
                  <option value="">Choose an upstream output…</option>
                  {workflowOutputs.map(output => <option key={`${output.file}:${output.stage}:${output.path}`} value={output.path}>{output.file} · {output.stage} → {output.path}</option>)}
                </NativeSelect>
              </label>
            )}
            <button
              type="button"
              className="text-xs text-primary hover:underline"
              onClick={addDep}
            >
              + Add dependency
            </button>
            <div className="mt-1 text-[10px] text-muted-foreground/60">
              Tip: Drag a connection from another stage on the canvas to add a stage dependency visually.
            </div>
          </div>
        </Section>

        {/* Outputs section */}
        <Section
          id="outs"
          label={`Outputs (${stage.outs.length})`}
          expanded={expandedSections.has("outs")}
          onToggle={toggleSection}
        >
          <div className="space-y-1">
            <div className="mb-2 text-[10px] text-muted-foreground/80">
              Files or directories produced by this stage. Outputs are tracked for caching and can be used as dependencies by downstream stages.
            </div>
            {stage.outs.map((out, i) => (
              <div key={i} className="group flex items-center gap-1">
                <span className="rounded bg-background px-1 py-0.5 text-[10px] text-muted-foreground">
                  {out.kind}
                </span>
                <span className="flex-1 truncate font-mono text-xs text-foreground">
                  {out.path}
                </span>
                <button
                  type="button"
                  className="opacity-0 group-hover:opacity-100"
                  onClick={() => removeOut(i)}
                >
                  <X size={12} className="text-muted-foreground hover:text-destructive" />
                </button>
              </div>
            ))}
            {stage.outs.length === 0 && (
              <div className="rounded bg-background/50 px-2 py-1.5 text-[10px] text-muted-foreground/60 italic">
                No outputs declared. Other stages cannot depend on this stage's results.
              </div>
            )}
            <button
              type="button"
              className="text-xs text-primary hover:underline"
              onClick={addOut}
            >
              + Add output
            </button>
            <div className="mt-1 text-[10px] text-muted-foreground/60">
              Outputs are hashed to determine if the stage needs re-running.
            </div>
          </div>
        </Section>

        {/* Params section */}
        <Section
          id="params"
          label={`Parameters (${stage.params.length})`}
          expanded={expandedSections.has("params")}
          onToggle={toggleSection}
        >
          <div className="space-y-1">
            <div className="mb-2 text-[10px] text-muted-foreground/80">
              Configuration values (from params files) that affect this stage. Changes to tracked parameters trigger re-execution.
            </div>
            {stage.params.map((param, i) => (
              <div key={i} className="group flex items-center gap-1">
                <span className="flex-1 truncate font-mono text-xs text-foreground">
                  {typeof param === "string" ? param : JSON.stringify(param)}
                </span>
                <button
                  type="button"
                  className="opacity-0 group-hover:opacity-100"
                  onClick={() => removeParam(i)}
                >
                  <X size={12} className="text-muted-foreground hover:text-destructive" />
                </button>
              </div>
            ))}
            {stage.params.length === 0 && (
              <div className="rounded bg-background/50 px-2 py-1.5 text-[10px] text-muted-foreground/60 italic">
                No parameters tracked. Hyperparameter changes won't trigger re-runs.
              </div>
            )}
            <button
              type="button"
              className="text-xs text-primary hover:underline"
              onClick={addParam}
            >
              + Add parameter
            </button>
            <div className="mt-1 text-[10px] text-muted-foreground/60">
              Use dotted paths like <code className="rounded bg-background px-0.5">model.lr</code> or <code className="rounded bg-background px-0.5">train.epochs</code>.
            </div>
          </div>
        </Section>

        {/* Options section */}
        <Section
          id="options"
          label="Options"
          expanded={expandedSections.has("options")}
          onToggle={toggleSection}
        >
          <div className="space-y-2">
            <div className="text-[10px] text-muted-foreground/80">
              Execution behavior flags that control how this stage is cached and run.
            </div>
            <label className="flex items-center gap-2 text-xs text-foreground">
              <input
                type="checkbox"
                checked={stage.nondeterministic ?? false}
                onChange={(e) =>
                  onUpdate({ ...stage, nondeterministic: e.target.checked })
                }
                className="rounded border-border"
              />
              Nondeterministic
            </label>
            <div className="ml-5 -mt-1 text-[10px] text-muted-foreground/60">
              Output may differ between runs even with same inputs (e.g. random seeds).
            </div>
            <label className="flex items-center gap-2 text-xs text-foreground">
              <input
                type="checkbox"
                checked={stage.hermetic ?? false}
                onChange={(e) =>
                  onUpdate({ ...stage, hermetic: e.target.checked })
                }
                className="rounded border-border"
              />
              Hermetic
            </label>
            <div className="ml-5 -mt-1 text-[10px] text-muted-foreground/60">
              Stage only accesses declared deps/outs. Enables stricter caching.
            </div>
            <label className="flex items-center gap-2 text-xs text-foreground">
              <input
                type="checkbox"
                checked={stage.side_effects ?? false}
                onChange={(e) =>
                  onUpdate({ ...stage, side_effects: e.target.checked })
                }
                className="rounded border-border"
              />
              Side effects
            </label>
            <div className="ml-5 -mt-1 text-[10px] text-muted-foreground/60">
              Stage modifies external state (e.g. deploys, sends notifications).
            </div>
            {stage.timeout !== undefined && (
              <div>
                <label className="text-[10px] text-muted-foreground">Timeout</label>
                <Input
                  type="text"
                  className="w-full text-xs"
                  value={stage.timeout ?? ""}
                  onChange={(e) =>
                    onUpdate({ ...stage, timeout: e.target.value || undefined })
                  }
                  placeholder="e.g. 30m, 6h"
                />
              </div>
            )}
          </div>
        </Section>

        {/* Frozen section */}
        <Section
          id="frozen"
          label="Frozen"
          expanded={expandedSections.has("frozen")}
          onToggle={toggleSection}
        >
          <label className="flex items-center gap-2 text-xs text-foreground">
            <input
              type="checkbox"
              checked={stage.frozen ?? false}
              onChange={(e) =>
                onUpdate({ ...stage, frozen: e.target.checked })
              }
              className="rounded border-border"
            />
            <Snowflake size={12} className="text-info" />
            Freeze this stage
          </label>
          <div className="mt-1 text-[10px] text-muted-foreground">
            Frozen stages are never re-run, even when dependencies change.
          </div>
        </Section>

        {/* Working Directory section */}
        <Section
          id="wdir"
          label="Working Directory"
          expanded={expandedSections.has("wdir")}
          onToggle={toggleSection}
        >
          <div className="flex items-center gap-2">
            <FolderOpen size={12} className="shrink-0 text-muted-foreground" />
            <Input
              type="text"
              className="w-full font-mono text-xs"
              value={stage.wdir ?? ""}
              onChange={(e) =>
                onUpdate({ ...stage, wdir: e.target.value || undefined })
              }
              placeholder="Relative path (default: repo root)"
            />
          </div>
          <div className="mt-1 text-[10px] text-muted-foreground">
            Commands run relative to this directory.
          </div>
        </Section>

        {/* Condition section */}
        <Section
          id="condition"
          label="Condition"
          expanded={expandedSections.has("condition")}
          onToggle={toggleSection}
        >
          <div className="mb-2 text-[10px] text-muted-foreground/80">
            Optional guard that determines whether this stage should run. If the condition evaluates to false, the stage is skipped.
          </div>
          <ConditionBuilder
            condition={stage.condition}
            onChange={(condition: ConditionSpec | undefined) =>
              onUpdate({ ...stage, condition })
            }
          />
        </Section>

        {/* Description section */}
        <Section
          id="desc"
          label="Description"
          expanded={expandedSections.has("desc")}
          onToggle={toggleSection}
        >
          <div className="mb-2 text-[10px] text-muted-foreground/80">
            A human-readable note explaining what this stage does. Shown in status reports and documentation.
          </div>
          <Textarea
            className="w-full resize-y text-xs"
            rows={3}
            value={stage.desc ?? ""}
            onChange={(e) =>
              onUpdate({ ...stage, desc: e.target.value || undefined })
            }
            placeholder="Describe what this stage does..."
          />
        </Section>

        {/* Metadata section */}
        <Section
          id="metadata"
          label="Metadata"
          expanded={expandedSections.has("metadata")}
          onToggle={toggleSection}
        >
          <div className="mb-2 text-[10px] text-muted-foreground/80">
            Custom key-value pairs for tagging or categorizing this stage. Metadata does not affect execution.
          </div>
          <MetadataEditor
            meta={(stage.meta as Record<string, string>) ?? {}}
            onChange={(meta) =>
              onUpdate({
                ...stage,
                meta: Object.keys(meta).length > 0 ? meta : undefined,
              })
            }
          />
        </Section>
      </div>
    </aside>
  );
}

function MetadataEditor({
  meta,
  onChange,
}: {
  meta: Record<string, string>;
  onChange: (meta: Record<string, string>) => void;
}) {
  const entries = Object.entries(meta);

  const handleAdd = useCallback(() => {
    let idx = entries.length + 1;
    let key = `key_${idx}`;
    while (key in meta) {
      idx++;
      key = `key_${idx}`;
    }
    onChange({ ...meta, [key]: "" });
  }, [meta, entries.length, onChange]);

  const handleRemove = useCallback(
    (key: string) => {
      const updated = { ...meta };
      delete updated[key];
      onChange(updated);
    },
    [meta, onChange],
  );

  const handleKeyChange = useCallback(
    (oldKey: string, newKey: string) => {
      if (newKey === oldKey || !newKey.trim()) return;
      const updated: Record<string, string> = {};
      for (const [k, v] of Object.entries(meta)) {
        if (k === oldKey) updated[newKey] = v;
        else updated[k] = v;
      }
      onChange(updated);
    },
    [meta, onChange],
  );

  const handleValueChange = useCallback(
    (key: string, value: string) => {
      onChange({ ...meta, [key]: value });
    },
    [meta, onChange],
  );

  return (
    <div className="space-y-1.5">
      {entries.map(([key, value]) => (
        <div key={key} className="group flex items-center gap-1">
          <Input
            className="w-20 shrink-0 font-mono text-xs"
            defaultValue={key}
            onBlur={(e) => handleKeyChange(key, e.target.value)}
            placeholder="key"
          />
          <span className="text-xs text-muted-foreground">:</span>
          <Input
            className="min-w-0 flex-1 font-mono text-xs"
            value={value}
            onChange={(e) => handleValueChange(key, e.target.value)}
            placeholder="value"
          />
          <button
            type="button"
            className="rounded p-0.5 text-muted-foreground opacity-0 hover:text-destructive group-hover:opacity-100"
            onClick={() => handleRemove(key)}
          >
            <X size={12} />
          </button>
        </div>
      ))}
      <button
        type="button"
        className="flex items-center gap-1 text-xs text-primary hover:underline"
        onClick={handleAdd}
      >
        <Plus size={12} /> Add metadata
      </button>
    </div>
  );
}

function Section({
  id,
  label,
  expanded,
  onToggle,
  children,
}: {
  id: string;
  label: string;
  expanded: boolean;
  onToggle: (id: string) => void;
  children: React.ReactNode;
}) {
  return (
    <div className="border-b border-border">
      <button
        type="button"
        className="flex w-full items-center gap-1 px-3 py-2 text-left text-xs font-medium text-foreground hover:bg-accent"
        onClick={() => onToggle(id)}
      >
        {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        {label}
      </button>
      {expanded && <div className="px-3 pb-3">{children}</div>}
    </div>
  );
}
