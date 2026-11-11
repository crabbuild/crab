import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { useCallback, useEffect, useRef, useState } from "react";
import { FileText, FolderOpen, Terminal, X } from "lucide-react";
import { publishToast } from "../../lib/toasts";

// ── Types ───────────────────────────────────────────────────────────

interface Props {
  /** Whether the dialog is visible. */
  open: boolean;
  /** Called to close the dialog. */
  onClose: () => void;
  /** Called after a command is successfully created (to refresh the list). */
  onCreated?: (name: string) => void;
  /** Pre-fill the name field (e.g. from the filter text). */
  initialName?: string;
}

type Scope = "repo" | "user";

// ── Component ───────────────────────────────────────────────────────

/**
 * Modal dialog for creating a new custom slash command.
 * Guides the user through name, description, scope, and template.
 */
export function CreateCommandDialog({ open, onClose, onCreated, initialName = "" }: Props) {
  const [name, setName] = useState(initialName);
  const [description, setDescription] = useState("");
  const [argumentHint, setArgumentHint] = useState("");
  const [category, setCategory] = useState("");
  const [scope, setScope] = useState<Scope>("repo");
  const [template, setTemplate] = useState("");
  const [creating, setCreating] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);

  // Focus the name input on open.
  useEffect(() => {
    if (open) {
      setName(initialName);
      setDescription("");
      setArgumentHint("");
      setCategory("");
      setTemplate("");
      setScope("repo");
      setTimeout(() => nameRef.current?.focus(), 50);
    }
  }, [open, initialName]);

  const handleCreate = useCallback(async () => {
    const trimmedName = name.trim().toLowerCase().replace(/[^a-z0-9-]/g, "-");
    if (!trimmedName) {
      publishToast({ kind: "error", title: "Name required", body: "Enter a command name." });
      return;
    }
    if (!description.trim()) {
      publishToast({ kind: "error", title: "Description required", body: "Enter a short description." });
      return;
    }

    setCreating(true);

    // Build the markdown content with frontmatter.
    const lines: string[] = ["---"];
    lines.push(`description: "${description.trim()}"`);
    if (argumentHint.trim()) lines.push(`argument-hint: "${argumentHint.trim()}"`);
    if (category.trim()) lines.push(`category: ${category.trim()}`);
    lines.push("---");
    lines.push("");
    lines.push(template.trim() || `# /${trimmedName}\n\n$ARGUMENTS`);
    lines.push("");

    const content = lines.join("\n");

    try {
      const result = await (window as any).crab?.orchestrator?.createCommand(
        trimmedName,
        scope,
        content,
      );

      if (result?.success) {
        publishToast({
          kind: "success",
          title: "Command created",
          body: `/${trimmedName} is now available.`,
        });
        onCreated?.(trimmedName);
        onClose();
      } else {
        publishToast({
          kind: "error",
          title: "Failed to create command",
          body: result?.error ?? "Unknown error",
        });
      }
    } catch (err: any) {
      publishToast({ kind: "error", title: "Error", body: err?.message ?? "Unknown error" });
    } finally {
      setCreating(false);
    }
  }, [name, description, argumentHint, category, scope, template, onCreated, onClose]);

  // Handle Enter in the name/description fields to advance focus.
  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        handleCreate();
      }
    },
    [onClose, handleCreate],
  );

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-[100] flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}
      onKeyDown={handleKeyDown}
    >
      <div className="w-full max-w-lg rounded-xl border border-border bg-background shadow-2xl">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-5 py-3">
          <div className="flex items-center gap-2">
            <Terminal size={14} className="text-primary" />
            <h2 className="text-sm font-semibold text-foreground">Create Slash Command</h2>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground transition"
          >
            <X size={14} />
          </button>
        </div>

        {/* Body */}
        <div className="space-y-4 px-5 py-4">
          {/* Name */}
          <div>
            <label className="block text-[11px] font-medium text-muted-foreground mb-1">
              Command name
            </label>
            <div className="flex items-center gap-1 rounded-md border border-border bg-card px-3 py-1.5">
              <span className="text-xs text-muted-foreground">/</span>
              <Input
                ref={nameRef}
                type="text"
                value={name}
                onChange={(e) => setName(e.target.value.replace(/[^a-zA-Z0-9-]/g, ""))}
                placeholder="my-command"
                className="flex-1 bg-transparent text-xs text-foreground outline-none placeholder:text-muted-foreground/50"
              />
            </div>
            <p className="mt-1 text-[10px] text-muted-foreground">
              Lowercase letters, numbers, and hyphens only.
            </p>
          </div>

          {/* Description */}
          <div>
            <label className="block text-[11px] font-medium text-muted-foreground mb-1">
              Description
            </label>
            <Input
              type="text"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="What does this command do?"
              className="w-full rounded-md border border-border bg-card px-3 py-1.5 text-xs text-foreground outline-none placeholder:text-muted-foreground/50 focus:border-primary/50"
            />
          </div>

          {/* Argument hint + Category (side by side) */}
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="block text-[11px] font-medium text-muted-foreground mb-1">
                Argument hint <span className="text-muted-foreground/50">(optional)</span>
              </label>
              <Input
                type="text"
                value={argumentHint}
                onChange={(e) => setArgumentHint(e.target.value)}
                placeholder="[filename] [format]"
                className="w-full rounded-md border border-border bg-card px-3 py-1.5 text-xs text-foreground outline-none placeholder:text-muted-foreground/50 focus:border-primary/50"
              />
            </div>
            <div>
              <label className="block text-[11px] font-medium text-muted-foreground mb-1">
                Category <span className="text-muted-foreground/50">(optional)</span>
              </label>
              <Input
                type="text"
                value={category}
                onChange={(e) => setCategory(e.target.value)}
                placeholder="git, review, testing…"
                className="w-full rounded-md border border-border bg-card px-3 py-1.5 text-xs text-foreground outline-none placeholder:text-muted-foreground/50 focus:border-primary/50"
              />
            </div>
          </div>

          {/* Scope */}
          <div>
            <label className="block text-[11px] font-medium text-muted-foreground mb-1.5">
              Save to
            </label>
            <div className="flex gap-2">
              <ScopeButton
                active={scope === "repo"}
                onClick={() => setScope("repo")}
                icon={<FolderOpen size={12} />}
                label="This project"
                hint=".crab/orchestrator/commands/"
              />
              <ScopeButton
                active={scope === "user"}
                onClick={() => setScope("user")}
                icon={<FileText size={12} />}
                label="All projects"
                hint="~/.crab/commands/"
              />
            </div>
          </div>

          {/* Template */}
          <div>
            <label className="block text-[11px] font-medium text-muted-foreground mb-1">
              Prompt template
            </label>
            <Textarea
              value={template}
              onChange={(e) => setTemplate(e.target.value)}
              placeholder={"Use $ARGUMENTS for the user's input.\nUse $0, $1 for positional args.\nUse !`command` for dynamic context injection."}
              rows={5}
              className="w-full rounded-md border border-border bg-card px-3 py-2 text-xs text-foreground font-mono outline-none placeholder:text-muted-foreground/50 focus:border-primary/50 resize-none"
            />
            <p className="mt-1 text-[10px] text-muted-foreground">
              Supports <code className="font-mono text-foreground/70">$ARGUMENTS</code>,{" "}
              <code className="font-mono text-foreground/70">$0</code>/<code className="font-mono text-foreground/70">$1</code> positional args, and{" "}
              <code className="font-mono text-foreground/70">!`shell command`</code> for dynamic context.
            </p>
          </div>
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 border-t border-border px-5 py-3">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-xs text-muted-foreground hover:bg-accent transition"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={handleCreate}
            disabled={creating || !name.trim() || !description.trim()}
            className="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50 disabled:cursor-not-allowed transition"
          >
            {creating ? "Creating…" : "Create command"}
          </button>
        </div>
      </div>
    </div>
  );
}

// ── Scope Button ────────────────────────────────────────────────────

function ScopeButton({
  active,
  onClick,
  icon,
  label,
  hint,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
  hint: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`flex-1 rounded-md border px-3 py-2 text-left transition ${
        active
          ? "border-primary/50 bg-primary/5"
          : "border-border bg-card hover:border-border/80"
      }`}
    >
      <div className="flex items-center gap-1.5">
        <span className={active ? "text-primary" : "text-muted-foreground"}>{icon}</span>
        <span className={`text-xs font-medium ${active ? "text-foreground" : "text-muted-foreground"}`}>
          {label}
        </span>
      </div>
      <div className="mt-0.5 text-[9px] font-mono text-muted-foreground/70 truncate">{hint}</div>
    </button>
  );
}
