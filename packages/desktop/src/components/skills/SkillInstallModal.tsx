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
import { useEffect, useState } from "react";
import { githubUrlToRaw } from "../../../electron/skill-validation";
import { publishToast } from "../../lib/toasts";
import { useSkills } from "../../state/skills-store";

export function SkillInstallModal({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const { installFromUrl } = useSkills();
  const [url, setUrl] = useState("");
  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setUrl("");
      setError(null);
    }
  }, [open]);

  async function handleInstall() {
    if (installing) return;
    if (!githubUrlToRaw(url)) {
      setError(
        "Enter a GitHub file or folder URL, a raw GitHub URL, or owner/repo/path/to/SKILL.md.",
      );
      return;
    }
    setError(null);
    setInstalling(true);
    try {
      const result = await installFromUrl(url.trim());
      if (!result.success) {
        setError(result.error ?? "Could not install this skill.");
        return;
      }
      publishToast({
        kind: "success",
        title: "Skill installed",
        body: "Review its instructions and agent links below.",
      });
      onClose();
    } finally {
      setInstalling(false);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (!value && !installing) onClose();
      }}
    >
      <DialogContent
        onKeyDown={(event) => {
          if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
            event.preventDefault();
            void handleInstall();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>Install from GitHub</DialogTitle>
          <DialogDescription>
            Import a SKILL.md into your user library, then review its
            instructions before using it.
          </DialogDescription>
        </DialogHeader>
        <form
          className="space-y-4"
          onSubmit={(event) => {
            event.preventDefault();
            void handleInstall();
          }}
        >
          <div className="space-y-1.5">
            <label className="text-sm font-medium" htmlFor="skill-source">
              GitHub URL or path
            </label>
            <Input
              className="w-full"
              id="skill-source"
              autoComplete="off"
              spellCheck={false}
              placeholder="owner/repo/skills/code-review/SKILL.md"
              value={url}
              onChange={(event) => setUrl(event.target.value)}
              disabled={installing}
              aria-describedby="skill-source-hint"
            />
            <p id="skill-source-hint" className="text-xs text-muted-foreground">
              Paste a GitHub file or folder URL, a raw GitHub URL, or
              owner/repo/path/to/SKILL.md (uses the main branch).
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
              disabled={installing}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              variant="default"
              disabled={installing || !url.trim()}
            >
              {installing ? "Installing…" : "Install skill"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
