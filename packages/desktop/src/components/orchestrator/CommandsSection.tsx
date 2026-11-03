import { useCallback, useEffect, useState } from "react";
import { ExternalLink, Loader2, Plus, Terminal, Trash2 } from "lucide-react";
import type { SlashCommand } from "./SlashCommandDropdown";
import { CreateCommandDialog } from "./CreateCommandDialog";
import { publishToast } from "../../lib/toasts";

// ── Component ───────────────────────────────────────────────────────

/**
 * Settings section that lists all available slash commands.
 * Shows name, description, argument hints, source badge, and category.
 * Allows creating new commands and opening existing ones for editing.
 */
export function CommandsSection() {
  const [commands, setCommands] = useState<SlashCommand[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);

  const loadCommands = useCallback(() => {
    (window as any).crab?.orchestrator
      ?.listCommands?.("")
      .then((result: { success?: boolean; commands?: SlashCommand[] }) => {
        if (result?.success && result.commands) {
          setCommands(result.commands);
        }
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
  }, []);

  useEffect(() => {
    loadCommands();
  }, [loadCommands]);

  const handleDelete = useCallback(async (name: string) => {
    const confirmed = await (window as any).crab?.dialogConfirm?.(
      `Delete command /${name}? This cannot be undone.`,
    );
    if (!confirmed) return;

    const result = await (window as any).crab?.orchestrator?.deleteCommand?.(name);
    if (result?.success) {
      publishToast({ kind: "success", title: "Deleted", body: `/${name} removed.` });
      loadCommands();
    } else {
      publishToast({ kind: "error", title: "Delete failed", body: result?.error ?? "Unknown error" });
    }
  }, [loadCommands]);

  const handleOpenFile = useCallback(async (cmd: SlashCommand) => {
    if (cmd.path) {
      await (window as any).crab?.fsOpenExternal?.(cmd.path);
    } else if (cmd.source === "builtin") {
      publishToast({
        kind: "info",
        title: "Built-in command",
        body: "Override it by creating a command with the same name in your project or user directory.",
      });
    }
  }, []);

  if (!loaded) {
    return (
      <div className="flex items-center gap-2 text-xs text-gx-muted">
        <Loader2 size={11} className="animate-spin" /> Loading commands…
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {/* Header with create button */}
      <div className="flex items-center justify-between">
        <div className="text-[11px] text-gx-muted">
          {commands.length} command{commands.length !== 1 ? "s" : ""} available
        </div>
        <button
          type="button"
          onClick={() => setCreateOpen(true)}
          className="flex items-center gap-1 rounded-md bg-gx-accent/10 px-2 py-1 text-[10px] font-medium text-gx-accent hover:bg-gx-accent/20 transition"
        >
          <Plus size={10} />
          New command
        </button>
      </div>

      {/* Help text */}
      <div className="rounded-md border border-gx-border/50 bg-gx-surface/50 px-3 py-2 text-[10px] text-gx-muted leading-relaxed">
        <p className="mb-1">
          Commands are markdown files loaded from:
        </p>
        <ul className="list-disc list-inside space-y-0.5">
          <li>
            <code className="font-mono text-gx-text/70">.crab/orchestrator/commands/</code>{" "}
            <span className="text-gx-success">(project)</span>
          </li>
          <li>
            <code className="font-mono text-gx-text/70">~/.crab/commands/</code>{" "}
            <span className="text-gx-thinking">(user)</span>
          </li>
        </ul>
        <p className="mt-1.5">
          Use YAML frontmatter for metadata. Supports{" "}
          <code className="font-mono text-gx-text/70">$ARGUMENTS</code>,{" "}
          <code className="font-mono text-gx-text/70">$0/$1</code> positional args, and{" "}
          <code className="font-mono text-gx-text/70">!`shell`</code> dynamic context.
        </p>
      </div>

      {/* Command list */}
      {commands.length === 0 ? (
        <div className="text-xs text-gx-muted text-center py-4">
          No commands yet. Click "New command" to create one.
        </div>
      ) : (
        <div className="space-y-1.5">
          {commands.map((cmd) => (
            <div
              key={`${cmd.source}-${cmd.name}`}
              className="group flex items-center gap-2 rounded-md border border-gx-border/60 bg-gx-bg px-3 py-2 hover:border-gx-border transition"
            >
              <Terminal size={12} className="shrink-0 text-gx-accent" />
              <div className="min-w-0 flex-1">
                <div className="flex items-baseline gap-1.5">
                  <span className="text-xs font-medium text-gx-text">/{cmd.name}</span>
                  {cmd.argumentHint && (
                    <span className="text-[10px] font-mono text-gx-muted/70">
                      {cmd.argumentHint}
                    </span>
                  )}
                </div>
                <div className="text-[10px] text-gx-muted truncate">{cmd.description}</div>
              </div>

              {/* Category badge */}
              {cmd.category && (
                <span className="shrink-0 rounded-full bg-gx-surface px-1.5 py-0.5 text-[9px] text-gx-muted">
                  {cmd.category}
                </span>
              )}

              {/* Source badge */}
              <span
                className={`shrink-0 rounded-full px-2 py-0.5 text-[9px] ${
                  cmd.source === "builtin"
                    ? "bg-gx-accent/15 text-gx-accent"
                    : cmd.source === "repo"
                      ? "bg-gx-success/15 text-gx-success"
                      : "bg-gx-thinking/15 text-gx-thinking"
                }`}
              >
                {cmd.source}
              </span>

              {/* Actions (visible on hover) */}
              <div className="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
                {cmd.path && (
                  <button
                    type="button"
                    onClick={() => handleOpenFile(cmd)}
                    title="Open file"
                    className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
                  >
                    <ExternalLink size={11} />
                  </button>
                )}
                {cmd.source !== "builtin" && (
                  <button
                    type="button"
                    onClick={() => handleDelete(cmd.name)}
                    title="Delete command"
                    className="rounded p-1 text-gx-muted hover:bg-red-500/10 hover:text-red-400 transition"
                  >
                    <Trash2 size={11} />
                  </button>
                )}
              </div>
            </div>
          ))}
        </div>
      )}

      {/* Create dialog */}
      <CreateCommandDialog
        open={createOpen}
        onClose={() => setCreateOpen(false)}
        onCreated={() => loadCommands()}
      />
    </div>
  );
}
