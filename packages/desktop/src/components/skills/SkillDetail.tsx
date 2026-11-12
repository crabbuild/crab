import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { Check, Edit3, FolderGit2, Globe, Trash2, X } from "lucide-react";
import { marked } from "marked";
import { useState } from "react";
import { publishToast } from "../../lib/toasts";
import { useSkills } from "../../state/skills-store";

function renderMarkdown(text: string): string {
  if (!text) return "";
  return marked.parse(text, { async: false, breaks: true, gfm: true }) as string;
}

function LoadingSkeleton() {
  return (
    <div className="flex-1 overflow-y-auto p-4 space-y-6 animate-pulse">
      <div className="flex items-start justify-between">
        <div className="flex-1 space-y-2">
          <div className="h-6 bg-border rounded w-48" />
          <div className="h-4 bg-border rounded w-72" />
        </div>
        <div className="flex gap-1">
          <div className="h-8 w-8 bg-border rounded" />
          <div className="h-8 w-8 bg-border rounded" />
        </div>
      </div>
      <div className="space-y-2">
        <div className="h-3 bg-border rounded w-24" />
        <div className="grid grid-cols-2 gap-1.5">
          {Array.from({ length: 6 }).map((_, i) => (
            <div key={i} className="h-6 bg-border rounded" />
          ))}
        </div>
      </div>
      <div className="space-y-2">
        <div className="h-3 bg-border rounded w-24" />
        <div className="h-32 bg-border rounded" />
      </div>
    </div>
  );
}

export function SkillDetail() {
  const { state, toggleEnabled, deleteSkill, updateSkill } = useSkills();
  const skill = state.selectedSkill;
  const [editing, setEditing] = useState(false);
  const [editName, setEditName] = useState("");
  const [editDesc, setEditDesc] = useState("");
  const [editBody, setEditBody] = useState("");
  const [saving, setSaving] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);

  const startEdit = () => {
    if (!skill) return;
    setEditName(skill.frontmatter.name || skill.record.name);
    setEditDesc(skill.record.description);
    setEditBody(skill.body);
    setEditing(true);
  };

  const cancelEdit = () => setEditing(false);

  const saveEdit = async () => {
    if (!skill) return;
    setSaving(true);
    const result = await updateSkill({
      id: skill.record.id,
      updates: { name: editName, description: editDesc, body: editBody },
    });
    setSaving(false);
    if (result.success) {
      setEditing(false);
      publishToast({ kind: "success", title: "Skill updated", body: `${editName} saved.` });
    } else {
      publishToast({ kind: "error", title: "Update failed", body: result.error ?? "Unknown error" });
    }
  };

  const handleDelete = async () => {
    if (!skill) return;
    setDeleting(true);
    const result = await deleteSkill(skill.record.id);
    setDeleting(false);
    if (result.success) {
      publishToast({ kind: "success", title: "Skill deleted", body: `${skill.frontmatter.name} removed.` });
    } else {
      publishToast({ kind: "error", title: "Delete failed", body: result.error ?? "Unknown error" });
    }
    setConfirmDelete(false);
  };

  if (!skill) {
    return (
      <div className="flex-1 flex items-center justify-center text-sm text-muted-foreground">
        Select a skill to view details
      </div>
    );
  }

  if (state.detailLoading) {
    return <LoadingSkeleton />;
  }

  return (
    <div className="flex-1 overflow-y-auto p-4 space-y-6">
      {/* Header */}
      <div className="flex items-start justify-between">
        <div className="flex-1 min-w-0">
          {editing ? (
            <Input
              className="w-full text-lg font-semibold text-foreground bg-accent border border-border rounded px-2 py-1 focus:outline-none focus:border-primary"
              value={editName}
              onChange={(e) => setEditName(e.target.value)}
            />
          ) : (
            <h3 className="text-lg font-semibold text-foreground truncate">
              {skill.frontmatter.name || skill.record.name}
            </h3>
          )}
          {editing ? (
            <Input
              className="w-full text-xs text-muted-foreground mt-1 bg-accent border border-border rounded px-2 py-1 focus:outline-none focus:border-primary"
              value={editDesc}
              onChange={(e) => setEditDesc(e.target.value)}
            />
          ) : (
            <p className="text-xs text-muted-foreground mt-1">
              {skill.record.description}
            </p>
          )}
        </div>
        <div className="flex items-center gap-1 ml-4 shrink-0">
          {editing ? (
            <>
              <Button
                variant="ghost"
                size="compact"
                className="p-1.5 rounded hover:bg-success/10 text-success transition-colors"
                onClick={saveEdit}
                disabled={saving}
                title="Save"
              >
                <Check size={16} />
              </Button>
              <Button
                variant="ghost"
                size="compact"
                className="p-1.5 rounded hover:bg-destructive/10 text-destructive transition-colors"
                onClick={cancelEdit}
                title="Cancel"
              >
                <X size={16} />
              </Button>
            </>
          ) : confirmDelete ? (
            <>
              <span className="text-xs text-destructive font-medium mr-1">
                Delete?
              </span>
              <Button
                variant="destructive"
                size="compact"
                className="p-1.5 rounded bg-destructive text-destructive-foreground hover:opacity-90 transition-opacity"
                onClick={handleDelete}
                disabled={deleting}
                title="Confirm delete"
              >
                <Check size={16} />
              </Button>
              <Button
                variant="ghost"
                size="compact"
                className="p-1.5 rounded hover:bg-accent text-muted-foreground hover:text-foreground transition-colors"
                onClick={() => setConfirmDelete(false)}
                title="Cancel"
              >
                <X size={16} />
              </Button>
            </>
          ) : (
            <>
              <Button
                variant="ghost"
                size="compact"
                className="p-1.5 rounded hover:bg-accent text-muted-foreground hover:text-foreground transition-colors"
                onClick={startEdit}
                title="Edit skill"
              >
                <Edit3 size={16} />
              </Button>
              <Button
                variant="ghost"
                size="compact"
                className="p-1.5 rounded hover:bg-destructive/10 text-muted-foreground hover:text-destructive transition-colors"
                onClick={() => setConfirmDelete(true)}
                title="Delete skill"
              >
                <Trash2 size={16} />
              </Button>
            </>
          )}
        </div>
      </div>

      {/* Agent Sync Status */}
      <div className="flex items-center gap-2">
        <Switch
          checked={skill.record.enabled}
          onCheckedChange={() => toggleEnabled(skill.record.id, !skill.record.enabled)}
        />
        <span className="text-xs text-muted-foreground">
          {skill.record.enabled ? "Enabled" : "Disabled"}
        </span>
      </div>

      {/* Agent Sync Status */}
      <section>
        <h4 className="text-xs font-semibold text-muted-foreground uppercase tracking-wider mb-2">
          Agent Availability
        </h4>
        <div className="grid grid-cols-2 gap-1.5">
          {Object.entries(skill.agentStatus).map(([agentId, status]) => (
            <div
              key={agentId}
              className="flex items-center gap-2 text-xs px-2 py-1 rounded"
            >
              <span
                className={`h-2 w-2 rounded-full shrink-0 ${
                  status === "synced"
                    ? "bg-success"
                    : status === "agent_not_installed"
                      ? "bg-warning"
                      : "bg-destructive"
                }`}
              />
              <span className="text-foreground">{agentId}</span>
              <span className="text-muted-foreground ml-auto text-[10px]">
                {status === "synced" ? "synced" : status === "agent_not_installed" ? "n/a" : "error"}
              </span>
            </div>
          ))}
        </div>
      </section>

      {/* Body content */}
      <section>
        <h4 className="text-xs font-semibold text-muted-foreground uppercase tracking-wider mb-2">
          Skill Content
        </h4>
        {editing ? (
          <Textarea
            className="w-full h-48 rounded border border-border bg-accent px-3 py-2 text-xs text-foreground font-mono focus:outline-none focus:border-primary resize-y"
            value={editBody}
            onChange={(e) => setEditBody(e.target.value)}
          />
        ) : (
          <div
            className="text-xs text-foreground bg-accent rounded p-3 prose prose-xs prose-invert max-w-none max-h-64 overflow-y-auto"
            dangerouslySetInnerHTML={{ __html: renderMarkdown(skill.body) || "<span class='text-muted-foreground'>(empty)</span>" }}
          />
        )}
      </section>

      {/* Metadata */}
      <section>
        <h4 className="text-xs font-semibold text-muted-foreground uppercase tracking-wider mb-2">
          Details
        </h4>
        <div className="space-y-1 text-xs">
          <div className="flex items-center gap-2">
            {skill.record.scope === "user" ? (
              <Globe size={12} className="text-muted-foreground" />
            ) : (
              <FolderGit2 size={12} className="text-muted-foreground" />
            )}
            <span className="text-muted-foreground">Scope:</span>
            <span className="text-foreground">{skill.record.scope}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">Installed:</span>
            <span className="text-foreground">
              {new Date(skill.record.installedAt).toLocaleDateString()}
            </span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-muted-foreground">Updated:</span>
            <span className="text-foreground">
              {new Date(skill.record.updatedAt).toLocaleDateString()}
            </span>
          </div>
          {skill.record.sourceUrl && (
            <div className="flex items-center gap-2">
              <span className="text-muted-foreground">Source:</span>
              <span className="text-foreground truncate">
                {skill.record.sourceUrl}
              </span>
            </div>
          )}
        </div>
      </section>
    </div>
  );
}
