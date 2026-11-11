import { Input } from "@/components/ui/input";
import { useCallback, useEffect, useState } from "react";
import { Loader2, Pencil, Trash2 } from "lucide-react";
import type { WorkspaceTemplate } from "./TemplatePicker";

// ── Component ───────────────────────────────────────────────────────

/**
 * Templates management section for OrchestratorSettings.
 *
 * Lists all loaded templates (built-in, repo-level, user-level).
 * Repo-level and built-in templates are read-only in the UI.
 * User-level templates can be edited (name/description) or deleted.
 */
export function TemplatesSection() {
  const [templates, setTemplates] = useState<WorkspaceTemplate[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [editingName, setEditingName] = useState<string | null>(null);
  const [editDesc, setEditDesc] = useState("");

  // Load templates on mount.
  useEffect(() => {
    const listFn = (window as any).crab?.orchestrator?.listTemplates;
    if (!listFn) {
      // IPC not wired — templates feature not available.
      setLoaded(true);
      return;
    }
    listFn()
      .then((result: any) => {
        if (result?.success && result.templates) {
          setTemplates(result.templates);
        }
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
  }, []);

  const handleDelete = useCallback(async (name: string) => {
    try {
      const result = await (window as any).crab?.orchestrator?.deleteTemplate?.(name);
      if (result?.success) {
        setTemplates((prev) => prev.filter((t) => t.name !== name));
      }
    } catch {
      // Best-effort deletion.
    }
  }, []);

  const handleEditStart = useCallback((template: WorkspaceTemplate) => {
    setEditingName(template.name);
    setEditDesc(template.description);
  }, []);

  const handleEditSave = useCallback(async () => {
    if (!editingName) return;
    const template = templates.find((t) => t.name === editingName);
    if (!template) return;

    try {
      const updated = { ...template, description: editDesc.trim() };
      await (window as any).crab?.orchestrator?.saveTemplate?.(updated);
      setTemplates((prev) =>
        prev.map((t) => (t.name === editingName ? { ...t, description: editDesc.trim() } : t)),
      );
    } catch {
      // Best-effort save.
    }
    setEditingName(null);
  }, [editingName, editDesc, templates]);

  if (!loaded) {
    return (
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        <Loader2 size={11} className="animate-spin" /> Loading templates…
      </div>
    );
  }

  if (templates.length === 0) {
    return (
      <div className="text-xs text-muted-foreground">
        No templates configured. Use "Save as template" from a workspace to create one, or add
        templates to <code className="font-mono text-foreground/80">crab-orchestrator.json</code>.
      </div>
    );
  }

  return (
    <div className="space-y-2">
      {templates.map((template) => {
        const isEditing = editingName === template.name;
        const isUserLevel = template.source === "user";

        return (
          <div
            key={template.name}
            className="flex items-center gap-2 rounded border border-border/60 bg-background px-3 py-2"
          >
            {/* Icon */}
            {template.icon && (
              <span className="shrink-0 text-base" aria-hidden="true">
                {template.icon}
              </span>
            )}

            {/* Name + description */}
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1.5">
                <span className="text-xs font-medium text-foreground">{template.name}</span>
                <span className="shrink-0 rounded-full bg-background px-1.5 py-0.5 text-[9px] text-muted-foreground">
                  {template.source}
                </span>
              </div>
              {isEditing ? (
                <Input
                  className="mt-1 w-full rounded border border-border bg-background px-2 py-0.5 text-[11px] text-foreground focus:border-primary focus:outline-none"
                  value={editDesc}
                  onChange={(e) => setEditDesc(e.target.value)}
                  onBlur={handleEditSave}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") handleEditSave();
                    if (e.key === "Escape") setEditingName(null);
                  }}
                  autoFocus
                />
              ) : (
                <div className="text-[10px] text-muted-foreground truncate">
                  {template.description || "No description"}
                </div>
              )}
            </div>

            {/* Actions (user-level only) */}
            {isUserLevel && !isEditing && (
              <div className="flex shrink-0 items-center gap-1">
                <button
                  type="button"
                  onClick={() => handleEditStart(template)}
                  className="rounded p-1 text-muted-foreground transition hover:bg-accent hover:text-foreground"
                  aria-label={`Edit ${template.name}`}
                >
                  <Pencil size={12} />
                </button>
                <button
                  type="button"
                  onClick={() => handleDelete(template.name)}
                  className="rounded p-1 text-muted-foreground transition hover:bg-destructive/10 hover:text-destructive"
                  aria-label={`Delete ${template.name}`}
                >
                  <Trash2 size={12} />
                </button>
              </div>
            )}

            {/* Read-only badge for non-user templates */}
            {!isUserLevel && (
              <span className="shrink-0 text-[9px] text-muted-foreground">read-only</span>
            )}
          </div>
        );
      })}
    </div>
  );
}
