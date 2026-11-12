import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { publishToast } from "../../lib/toasts";
import { useSkills } from "../../state/skills-store";

function isValidName(name: string): boolean {
  return (
    /^[a-z0-9][a-z0-9-]{0,62}[a-z0-9]?$/.test(name) && !name.includes("--")
  );
}

interface Props {
  open: boolean;
  onClose: () => void;
}

export function SkillCreateModal({ open, onClose }: Props) {
  const { createSkill } = useSkills();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [body, setBody] = useState("");
  const [creating, setCreating] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) {
      setName("");
      setDescription("");
      setBody("");
      setCreating(false);
      setTimeout(() => nameRef.current?.focus(), 50);
    }
  }, [open]);

  const handleCreate = async () => {
    const trimmedName = name.trim();
    if (!trimmedName) {
      publishToast({ kind: "error", title: "Name required", body: "Enter a skill name." });
      return;
    }
    if (!isValidName(trimmedName)) {
      publishToast({
        kind: "error",
        title: "Invalid name",
        body: "Use lowercase letters, numbers, and hyphens (1-64 chars).",
      });
      return;
    }
    if (!description.trim()) {
      publishToast({ kind: "error", title: "Description required", body: "Enter a short description." });
      return;
    }

    setCreating(true);
    const result = await createSkill({
      name: trimmedName,
      description: description.trim(),
      body: body.trim() || undefined,
      scope: "user",
    });
    setCreating(false);

    if (result.success) {
      publishToast({ kind: "success", title: "Skill created", body: `${trimmedName} is ready.` });
      onClose();
    } else {
      publishToast({ kind: "error", title: "Failed to create", body: result.error ?? "Unknown error" });
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") handleCreate();
  };

  if (!open) return null;

  return (
    <Dialog
      open
      onOpenChange={(open) => { if (!open) onClose(); }}
    >
      <DialogContent
        onKeyDown={handleKeyDown}
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 w-full max-w-lg rounded-xl border border-border bg-background shadow-2xl"
      >
        {/* Header */}
        <div className="flex items-center justify-between px-4 py-3 border-b border-border">
          <DialogTitle className="text-sm font-semibold text-foreground">
            New Skill
          </DialogTitle>
          <Button
            variant="ghost"
            size="compact"
            className="p-1 rounded hover:bg-accent text-muted-foreground hover:text-foreground transition-colors"
            onClick={onClose}
          >
            <X size={16} />
          </Button>
        </div>

        {/* Body */}
        <div className="px-4 py-3 space-y-3">
          <div>
            <label className="block text-xs font-medium text-muted-foreground mb-1">
              Name
            </label>
            <Input
              ref={nameRef}
              type="text"
              placeholder="my-skill"
              value={name}
              onChange={(e) => setName(e.target.value)}
              className="w-full rounded border border-border bg-accent px-2 py-1.5 text-xs text-foreground placeholder:text-muted-foreground focus:outline-none focus:border-primary"
            />
            <p className="text-[10px] text-muted-foreground mt-0.5">
              Lowercase letters, numbers, and hyphens. 1-64 characters.
            </p>
          </div>
          <div>
            <label className="block text-xs font-medium text-muted-foreground mb-1">
              Description
            </label>
            <Input
              type="text"
              placeholder="A short description of what this skill does"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              className="w-full rounded border border-border bg-accent px-2 py-1.5 text-xs text-foreground placeholder:text-muted-foreground focus:outline-none focus:border-primary"
            />
          </div>
          <div>
            <label className="block text-xs font-medium text-muted-foreground mb-1">
              Body <span className="text-muted-foreground">(optional)</span>
            </label>
            <Textarea
              placeholder="Markdown content for the skill..."
              value={body}
              onChange={(e) => setBody(e.target.value)}
              rows={6}
              className="w-full rounded border border-border bg-accent px-2 py-1.5 text-xs text-foreground font-mono placeholder:text-muted-foreground focus:outline-none focus:border-primary resize-y"
            />
          </div>
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 px-4 py-3 border-t border-border">
          <Button
            variant="ghost"
            size="compact"
            className="rounded-md px-3 py-1.5 text-xs font-medium text-muted-foreground hover:text-foreground transition-colors"
            onClick={onClose}
          >
            Cancel
          </Button>
          <Button
            variant="default"
            size="compact"
            className="rounded-md bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground hover:opacity-90 transition-opacity disabled:opacity-50"
            onClick={handleCreate}
            disabled={creating}
          >
            {creating ? "Creating..." : "Create Skill"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
