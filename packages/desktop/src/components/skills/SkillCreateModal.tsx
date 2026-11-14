import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { useEffect, useState } from "react";
import { isValidSkillName } from "../../../electron/skill-validation";
import { publishToast } from "../../lib/toasts";
import { useSkills } from "../../state/skills-store";

export function SkillCreateModal({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const { createSkill } = useSkills();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [body, setBody] = useState("");
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setName("");
      setDescription("");
      setBody("");
      setError(null);
    }
  }, [open]);

  async function handleCreate() {
    if (creating) return;
    if (!isValidSkillName(name.trim())) {
      setError(
        "Use 1–64 lowercase letters, numbers or hyphens. Start with a letter or number; avoid consecutive hyphens.",
      );
      return;
    }
    if (!description.trim()) {
      setError("Add a description so you can find this skill later.");
      return;
    }
    setError(null);
    setCreating(true);
    try {
      const result = await createSkill({
        name: name.trim(),
        description: description.trim(),
        body: body.trim() || undefined,
        scope: "user",
      });
      if (!result.success) {
        setError(result.error ?? "Could not create this skill.");
        return;
      }
      publishToast({
        kind: "success",
        title: "Skill created",
        body: "Review its instructions and agent links below.",
      });
      onClose();
    } finally {
      setCreating(false);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (!value && !creating) onClose();
      }}
    >
      <DialogContent
        className="max-h-[90vh] overflow-y-auto"
        onKeyDown={(event) => {
          if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
            event.preventDefault();
            void handleCreate();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>New skill</DialogTitle>
          <DialogDescription>
            Save reusable Markdown instructions in your user library. Crab links
            them to the agent folders it finds.
          </DialogDescription>
        </DialogHeader>
        <form
          className="space-y-4"
          onSubmit={(event) => {
            event.preventDefault();
            void handleCreate();
          }}
        >
          <div className="space-y-1.5">
            <label className="text-sm font-medium" htmlFor="skill-name">
              Name
            </label>
            <Input
              className="w-full"
              id="skill-name"
              autoComplete="off"
              placeholder="code-review"
              value={name}
              onChange={(event) => setName(event.target.value)}
              aria-describedby="skill-name-hint"
              disabled={creating}
            />
            <p id="skill-name-hint" className="text-xs text-muted-foreground">
              Lowercase letters, numbers and hyphens; 1–64 characters.
            </p>
          </div>
          <div className="space-y-1.5">
            <label className="text-sm font-medium" htmlFor="skill-description">
              Description
            </label>
            <Input
              className="w-full"
              id="skill-description"
              placeholder="Review changes for bugs and missing tests"
              value={description}
              onChange={(event) => setDescription(event.target.value)}
              disabled={creating}
            />
          </div>
          <div className="space-y-1.5">
            <label className="text-sm font-medium" htmlFor="skill-instructions">
              Instructions{" "}
              <span className="font-normal text-muted-foreground">
                (optional)
              </span>
            </label>
            <Textarea
              id="skill-instructions"
              rows={7}
              className="font-mono text-xs"
              placeholder="Describe when to use this skill and the steps the agent should follow…"
              value={body}
              onChange={(event) => setBody(event.target.value)}
              disabled={creating}
            />
            <p className="text-xs text-muted-foreground">
              Supports Markdown. You can edit these instructions later.
            </p>
          </div>
          {error && (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}
          <DialogFooter>
            <Button
              type="button"
              variant="ghost"
              onClick={onClose}
              disabled={creating}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              variant="default"
              disabled={creating || !name.trim() || !description.trim()}
            >
              {creating ? "Creating…" : "Create skill"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
