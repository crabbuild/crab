import { useState } from "react";
import { Trash2, Edit3, Check, X, Globe, FolderGit2 } from "lucide-react";
import { useSkills } from "../../state/skills-store";
import { publishToast } from "../../lib/toasts";
import { marked } from "marked";

function renderMarkdown(text: string): string {
  if (!text) return "";
  return marked.parse(text, { async: false, breaks: true, gfm: true }) as string;
}

function LoadingSkeleton() {
  return (
    <div className="flex-1 overflow-y-auto p-4 space-y-6 animate-pulse">
      <div className="flex items-start justify-between">
        <div className="flex-1 space-y-2">
          <div className="h-6 bg-gx-border rounded w-48" />
          <div className="h-4 bg-gx-border rounded w-72" />
        </div>
        <div className="flex gap-1">
          <div className="h-8 w-8 bg-gx-border rounded" />
          <div className="h-8 w-8 bg-gx-border rounded" />
        </div>
      </div>
      <div className="space-y-2">
        <div className="h-3 bg-gx-border rounded w-24" />
        <div className="grid grid-cols-2 gap-1.5">
          {Array.from({ length: 6 }).map((_, i) => (
            <div key={i} className="h-6 bg-gx-border rounded" />
          ))}
        </div>
      </div>
      <div className="space-y-2">
        <div className="h-3 bg-gx-border rounded w-24" />
        <div className="h-32 bg-gx-border rounded" />
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
      <div className="flex-1 flex items-center justify-center text-sm text-gx-muted">
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
            <input
              className="w-full text-lg font-semibold text-gx-text bg-gx-surface-hover border border-gx-border rounded px-2 py-1 focus:outline-none focus:border-gx-accent"
              value={editName}
              onChange={(e) => setEditName(e.target.value)}
            />
          ) : (
            <h3 className="text-lg font-semibold text-gx-text truncate">
              {skill.frontmatter.name || skill.record.name}
            </h3>
          )}
          {editing ? (
            <input
              className="w-full text-xs text-gx-muted mt-1 bg-gx-surface-hover border border-gx-border rounded px-2 py-1 focus:outline-none focus:border-gx-accent"
              value={editDesc}
              onChange={(e) => setEditDesc(e.target.value)}
            />
          ) : (
            <p className="text-xs text-gx-muted mt-1">{skill.record.description}</p>
          )}
        </div>
        <div className="flex items-center gap-1 ml-4 shrink-0">
          {editing ? (
            <>
              <button
                className="p-1.5 rounded hover:bg-green-500/10 text-green-500 transition-colors"
                onClick={saveEdit}
                disabled={saving}
                title="Save"
              >
                <Check size={16} />
              </button>
              <button
                className="p-1.5 rounded hover:bg-gx-error/10 text-gx-error transition-colors"
                onClick={cancelEdit}
                title="Cancel"
              >
                <X size={16} />
              </button>
            </>
          ) : confirmDelete ? (
            <>
              <span className="text-xs text-gx-error font-medium mr-1">Delete?</span>
              <button
                className="p-1.5 rounded bg-gx-error text-white hover:opacity-90 transition-opacity"
                onClick={handleDelete}
                disabled={deleting}
                title="Confirm delete"
              >
                <Check size={16} />
              </button>
              <button
                className="p-1.5 rounded hover:bg-gx-surface-hover text-gx-muted hover:text-gx-text transition-colors"
                onClick={() => setConfirmDelete(false)}
                title="Cancel"
              >
                <X size={16} />
              </button>
            </>
          ) : (
            <>
              <button
                className="p-1.5 rounded hover:bg-gx-surface-hover text-gx-muted hover:text-gx-text transition-colors"
                onClick={startEdit}
                title="Edit skill"
              >
                <Edit3 size={16} />
              </button>
              <button
                className="p-1.5 rounded hover:bg-gx-error/10 text-gx-muted hover:text-gx-error transition-colors"
                onClick={() => setConfirmDelete(true)}
                title="Delete skill"
              >
                <Trash2 size={16} />
              </button>
            </>
          )}
        </div>
      </div>

      {/* Enabled toggle */}
      <div className="flex items-center gap-2">
        <button
          className={`relative inline-flex h-5 w-9 items-center rounded-full transition-colors ${
            skill.record.enabled ? "bg-gx-accent" : "bg-gx-border"
          }`}
          onClick={() => toggleEnabled(skill.record.id, !skill.record.enabled)}
          role="switch"
          aria-checked={skill.record.enabled}
        >
          <span
            className={`inline-block h-3.5 w-3.5 rounded-full bg-white transition-transform ${
              skill.record.enabled ? "translate-x-[18px]" : "translate-x-[2px]"
            }`}
          />
        </button>
        <span className="text-xs text-gx-muted">
          {skill.record.enabled ? "Enabled" : "Disabled"}
        </span>
      </div>

      {/* Agent Sync Status */}
      <section>
        <h4 className="text-xs font-semibold text-gx-muted uppercase tracking-wider mb-2">
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
                    ? "bg-green-500"
                    : status === "agent_not_installed"
                      ? "bg-yellow-600"
                      : "bg-red-500"
                }`}
              />
              <span className="text-gx-text">{agentId}</span>
              <span className="text-gx-muted ml-auto text-[10px]">
                {status === "synced" ? "synced" : status === "agent_not_installed" ? "n/a" : "error"}
              </span>
            </div>
          ))}
        </div>
      </section>

      {/* Body content */}
      <section>
        <h4 className="text-xs font-semibold text-gx-muted uppercase tracking-wider mb-2">
          Skill Content
        </h4>
        {editing ? (
          <textarea
            className="w-full h-48 rounded border border-gx-border bg-gx-surface-hover px-3 py-2 text-xs text-gx-text font-mono focus:outline-none focus:border-gx-accent resize-y"
            value={editBody}
            onChange={(e) => setEditBody(e.target.value)}
          />
        ) : (
          <div
            className="text-xs text-gx-text bg-gx-surface-hover rounded p-3 prose prose-xs prose-invert max-w-none max-h-64 overflow-y-auto"
            dangerouslySetInnerHTML={{ __html: renderMarkdown(skill.body) || "<span class='text-gx-muted'>(empty)</span>" }}
          />
        )}
      </section>

      {/* Metadata */}
      <section>
        <h4 className="text-xs font-semibold text-gx-muted uppercase tracking-wider mb-2">
          Details
        </h4>
        <div className="space-y-1 text-xs">
          <div className="flex items-center gap-2">
            {skill.record.scope === "user" ? <Globe size={12} className="text-gx-muted" /> : <FolderGit2 size={12} className="text-gx-muted" />}
            <span className="text-gx-muted">Scope:</span>
            <span className="text-gx-text">{skill.record.scope}</span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-gx-muted">Installed:</span>
            <span className="text-gx-text">
              {new Date(skill.record.installedAt).toLocaleDateString()}
            </span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-gx-muted">Updated:</span>
            <span className="text-gx-text">
              {new Date(skill.record.updatedAt).toLocaleDateString()}
            </span>
          </div>
          {skill.record.sourceUrl && (
            <div className="flex items-center gap-2">
              <span className="text-gx-muted">Source:</span>
              <span className="text-gx-text truncate">{skill.record.sourceUrl}</span>
            </div>
          )}
        </div>
      </section>
    </div>
  );
}
