import { useState } from "react";
import { AlertTriangle, ArrowRightLeft, ArrowDownLeft, ArrowUpRight, FileCode2, Pencil, Plus, RefreshCw, Search, Trash2, X } from "lucide-react";
import clsx from "clsx";
import type { WorkflowDefinition } from "../../lib/workflow-types";
import { workflowPathError, type WorkflowLink } from "../../lib/workflow-graph";

interface Props {
  files: string[];
  workflows: WorkflowDefinition[];
  fileErrors: Record<string, string>;
  activeFile: string | null;
  links: WorkflowLink[];
  disabled: boolean;
  canRename: boolean;
  dirty: boolean;
  onSelect: (path: string) => void;
  onRefresh: () => void;
  onMigrate: () => void;
  onCreate: (path: string) => Promise<boolean>;
  onRename: (oldPath: string, newPath: string) => Promise<boolean>;
  onDelete: (path: string) => Promise<boolean>;
}

export function WorkflowSidebar({ files, workflows, fileErrors, activeFile, links, disabled, canRename, dirty, onSelect, onRefresh, onMigrate, onCreate, onRename, onDelete }: Props) {
  const [query, setQuery] = useState("");
  const [form, setForm] = useState<{ kind: "create" | "rename" | "delete"; path: string } | null>(null);
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const begin = (kind: "create" | "rename" | "delete", path = "") => {
    setForm({ kind, path });
    setName(kind === "create" ? "pipeline.workflow.yaml" : path.split("/").at(-1) ?? "");
    setError(null);
  };
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!form) return;
    const prefix = form.kind === "rename" ? form.path.slice(0, form.path.lastIndexOf("/") + 1) : "";
    const path = prefix + name.trim();
    if (form.kind !== "delete") {
      const problem = workflowPathError(path);
      if (problem) { setError(problem); return; }
      if (form.kind === "rename" && name.includes("/")) { setError("Rename within the same folder to keep output paths unchanged."); return; }
      if (files.includes(path)) { setError("A workflow with this path already exists."); return; }
    }
    const ok = form.kind === "create" ? await onCreate(path)
      : form.kind === "rename" ? await onRename(form.path, path) : await onDelete(form.path);
    if (ok) { setForm(null); setQuery(""); }
  };
  const relationships = [
    { title: "Depends on", icon: ArrowDownLeft, items: links.filter(link => link.toFile === activeFile && link.fromFile !== activeFile), file: (link: WorkflowLink) => link.fromFile },
    { title: "Used by", icon: ArrowUpRight, items: links.filter(link => link.fromFile === activeFile && link.toFile !== activeFile), file: (link: WorkflowLink) => link.toFile },
  ];
  return (
    <aside aria-label="Workflows" className="flex w-56 shrink-0 flex-col border-r border-gx-border bg-gx-surface lg:w-64">
      <div className="flex h-10 shrink-0 items-center gap-2 border-b border-gx-border px-3">
        <h2 className="min-w-0 flex-1 text-xs font-medium text-gx-text">Workflows <span className="ml-1 text-gx-muted">{files.length}</span></h2>
        <button type="button" className="btn p-1.5" aria-label="Refresh workflows" disabled={disabled} onClick={onRefresh}><RefreshCw size={13} /></button>
        <button type="button" className="btn p-1.5" aria-label="New workflow" disabled={disabled} onClick={() => begin("create")}><Plus size={14} /></button>
      </div>
      <div className="relative m-3">
        <Search size={13} className="pointer-events-none absolute left-2 top-2 text-gx-muted" />
        <input aria-label="Filter workflows" className="input-field w-full pl-7 text-xs" placeholder="Find a workflow…" value={query} onChange={event => setQuery(event.target.value)} />
      </div>
      {form && (
        <form aria-label={`${form.kind === "create" ? "New" : form.kind === "rename" ? "Rename" : "Delete"} workflow`} onSubmit={submit} className="mx-3 mb-3 space-y-2 rounded border border-gx-border bg-gx-bg p-3">
          <div className="flex items-center justify-between gap-2 text-xs font-medium">
            {form.kind === "create" ? "New workflow" : form.kind === "rename" ? "Rename workflow" : "Delete workflow?"}
            <button type="button" aria-label="Close workflow form" className="rounded p-1 hover:bg-gx-surface-hover" onClick={() => setForm(null)}><X size={13} /></button>
          </div>
          {form.kind === "delete" ? (
            <p className="break-words text-xs text-gx-muted">Remove {form.path} from disk. Outputs remain. {links.some(link => link.fromFile === form.path && link.toFile !== form.path) && "Other workflows use its outputs; they will lose this producer."}</p>
          ) : (
            <>
              <input autoFocus aria-label="Workflow path" className="input-field w-full text-xs" value={name} disabled={disabled} onChange={event => setName(event.target.value)} />
              <p className="text-xs text-gx-muted">{form.kind === "create" ? "Use crab.yaml or name.workflow.yaml." : "Stage namespace changes; the next run recomputes its cache."}</p>
            </>
          )}
          {error && <p role="alert" className="text-xs text-gx-error">{error}</p>}
          <button type="submit" disabled={disabled} className={clsx("btn w-full text-xs", form.kind === "delete" ? "btn-danger" : "btn-primary")}>
            {form.kind === "create" ? "Create workflow" : form.kind === "rename" ? "Rename workflow" : "Delete workflow"}
          </button>
        </form>
      )}
      <nav aria-label="Workflow files" className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {files.filter(file => file.toLowerCase().includes(query.toLowerCase())).map(file => {
          const workflow = workflows.find(workflow => workflow.path === file);
          const active = file === activeFile;
          return (
            <div key={file} className={clsx("group flex items-center rounded", active ? "bg-gx-accent/10" : "hover:bg-gx-surface-hover")}>
              <button type="button" disabled={disabled} aria-current={active ? "page" : undefined} title={fileErrors[file] ?? file} onClick={() => onSelect(file)} className={clsx("flex min-w-0 flex-1 items-start gap-2 rounded px-2 py-2 text-left", active ? "text-gx-accent" : "text-gx-text")}>
                {fileErrors[file] ? <AlertTriangle size={14} className="mt-0.5 shrink-0 text-gx-error" /> : <FileCode2 size={14} className="mt-0.5 shrink-0" />}
                <span className="min-w-0 flex-1"><span className="block truncate text-xs font-medium">{file}</span><span className="mt-0.5 block text-[11px] text-gx-muted">{fileErrors[file] ? "Cannot read workflow" : `${workflow?.stages.length ?? 0} ${workflow?.stages.length === 1 ? "stage" : "stages"}`}{active && dirty ? " · Unsaved" : ""}</span></span>
              </button>
              <div className="flex shrink-0 pr-1 opacity-0 focus-within:opacity-100 group-hover:opacity-100">
                {canRename && <button type="button" className="rounded p-1.5 text-gx-muted hover:text-gx-text" aria-label={`Rename ${file}`} disabled={disabled} onClick={() => begin("rename", file)}><Pencil size={12} /></button>}
                <button type="button" className="rounded p-1.5 text-gx-muted hover:text-gx-error" aria-label={`Delete ${file}`} disabled={disabled} onClick={() => begin("delete", file)}><Trash2 size={12} /></button>
              </div>
            </div>
          );
        })}
        {!files.length && <p className="px-2 py-4 text-xs text-gx-muted">Create a workflow to start building stages.</p>}
        {!!files.length && !files.some(file => file.toLowerCase().includes(query.toLowerCase())) && <p className="px-2 py-4 text-xs text-gx-muted">No matching workflows.</p>}
      </nav>
      {activeFile && (
        <div className="max-h-[35%] shrink-0 space-y-4 overflow-auto border-t border-gx-border p-3">
          {relationships.map(({ title, icon: Icon, items, file }) => (
            <section key={title} aria-label={title}>
              <h3 className="mb-1.5 flex items-center gap-1.5 text-xs font-medium text-gx-text"><Icon size={13} />{title}</h3>
              {!items.length && <p className="text-[11px] text-gx-muted">{title === "Depends on" ? "No upstream workflow outputs." : "No downstream workflows."}</p>}
              {[...new Set(items.map(file))].map(path => <button key={path} type="button" disabled={disabled} onClick={() => onSelect(path)} title={items.filter(item => file(item) === path).map(item => item.path).join("\n")} className="block max-w-full truncate py-1 text-xs text-gx-accent hover:underline">{path}</button>)}
            </section>
          ))}
        </div>
      )}
      <div className="shrink-0 space-y-2 border-t border-gx-border p-3">
        <button type="button" className="btn flex w-full items-center justify-center gap-2 text-xs" disabled={disabled} onClick={onMigrate}><ArrowRightLeft size={13} />Import DVC workflow</button>
        <p className="text-[11px] leading-relaxed text-gx-muted">Use output paths to connect workflows. Runs include upstream stages.</p>
      </div>
    </aside>
  );
}
