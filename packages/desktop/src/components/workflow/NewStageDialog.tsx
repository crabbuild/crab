import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { X } from "lucide-react";
import { useState } from "react";
import { WorkflowStage } from "../../lib/workflow-types";

interface Props {
  existingNames: string[];
  onAdd: (stage: WorkflowStage) => void;
  onClose: () => void;
}

export function NewStageDialog({ existingNames, onAdd, onClose }: Props) {
  const [name, setName] = useState("");
  const [cmd, setCmd] = useState("");
  const [error, setError] = useState<string | null>(null);

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);

    // Validate name.
    const trimmedName = name.trim();
    if (!trimmedName) {
      setError("Stage name is required.");
      return;
    }
    if (!/^[a-zA-Z_][a-zA-Z0-9_-]*$/.test(trimmedName)) {
      setError("Name must start with a letter or underscore, and contain only letters, digits, _ or -.");
      return;
    }
    if (trimmedName.length > 64) {
      setError("Name must be 64 characters or fewer.");
      return;
    }
    if (existingNames.includes(trimmedName)) {
      setError(`Stage "${trimmedName}" already exists.`);
      return;
    }

    const trimmedCmd = cmd.trim();
    if (!trimmedCmd) {
      setError("Command is required.");
      return;
    }

    const stage: WorkflowStage = {
      name: trimmedName,
      cmd: { kind: "shell", value: trimmedCmd },
      deps: [],
      outs: [],
      params: [],
      env: "inherit",
    };

    onAdd(stage);
  };

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
    >
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 w-[400px] rounded-lg border border-border bg-card shadow-xl"
      >
        <div className="flex items-center justify-between border-b border-border px-4 py-3">
          <DialogTitle className="text-sm font-medium text-foreground">
            New Stage
          </DialogTitle>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={onClose}
          >
            <X size={16} />
          </Button>
        </div>

        <form onSubmit={handleSubmit} className="p-4">
          <div className="space-y-3">
            <div>
              <label className="mb-1 block text-xs text-muted-foreground">
                Stage Name
              </label>
              <Input
                type="text"
                className="w-full text-sm"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="e.g. train, preprocess, evaluate"
                autoFocus
              />
            </div>

            <div>
              <label className="mb-1 block text-xs text-muted-foreground">
                Command
              </label>
              <Textarea
                className="w-full resize-y font-mono text-xs"
                rows={3}
                value={cmd}
                onChange={(e) => setCmd(e.target.value)}
                placeholder="python train.py --epochs 100"
              />
            </div>

            {error && (
              <div className="rounded border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
                {error}
              </div>
            )}
          </div>

          <div className="mt-4 flex justify-end gap-2">
            <Button variant="outline" type="button" className="text-xs" onClick={onClose}>
              Cancel
            </Button>
            <Button variant="default" type="submit" className="text-xs">
              Add Stage
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
