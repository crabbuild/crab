import { useState, useRef, useEffect } from "react";
import { X } from "lucide-react";
import { useSkills } from "../../state/skills-store";
import { publishToast } from "../../lib/toasts";

interface Props {
  open: boolean;
  onClose: () => void;
}

function isValidUrl(url: string): boolean {
  return (
    /^https?:\/\//.test(url) &&
    (url.includes("github.com") || url.includes("raw.githubusercontent.com"))
  );
}

export function SkillInstallModal({ open, onClose }: Props) {
  const { installFromUrl } = useSkills();
  const [url, setUrl] = useState("");
  const [installing, setInstalling] = useState(false);
  const urlRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) {
      setUrl("");
      setInstalling(false);
      setTimeout(() => urlRef.current?.focus(), 50);
    }
  }, [open]);

  const handleInstall = async () => {
    const trimmedUrl = url.trim();
    if (!trimmedUrl) {
      publishToast({ kind: "error", title: "URL required", body: "Enter a GitHub URL to a SKILL.md file." });
      return;
    }
    if (!isValidUrl(trimmedUrl)) {
      publishToast({
        kind: "error",
        title: "Invalid URL",
        body: "Enter a GitHub blob/tree URL or raw.githubusercontent.com URL.",
      });
      return;
    }

    setInstalling(true);
    const result = await installFromUrl(trimmedUrl);
    setInstalling(false);

    if (result.success) {
      publishToast({
        kind: "success",
        title: "Skill installed",
        body: `${result.data?.frontmatter.name ?? "Skill"} is ready.`,
      });
      onClose();
    } else {
      publishToast({ kind: "error", title: "Install failed", body: result.error ?? "Unknown error" });
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") onClose();
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") handleInstall();
  };

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-[100] flex items-center justify-center bg-black/50 backdrop-blur-sm"
      onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}
      onKeyDown={handleKeyDown}
    >
      <div className="w-full max-w-lg rounded-xl border border-gx-border bg-gx-bg shadow-2xl">
        {/* Header */}
        <div className="flex items-center justify-between px-4 py-3 border-b border-gx-border">
          <h3 className="text-sm font-semibold text-gx-text">Install Skill from URL</h3>
          <button
            className="p-1 rounded hover:bg-gx-surface-hover text-gx-muted hover:text-gx-text transition-colors"
            onClick={onClose}
          >
            <X size={16} />
          </button>
        </div>

        {/* Body */}
        <div className="px-4 py-3 space-y-3">
          <div>
            <label className="block text-xs font-medium text-gx-muted mb-1">GitHub URL</label>
            <input
              ref={urlRef}
              type="text"
              placeholder="https://github.com/owner/repo/blob/main/path/SKILL.md"
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              className="w-full rounded border border-gx-border bg-gx-surface-hover px-2 py-1.5 text-xs text-gx-text placeholder:text-gx-muted focus:outline-none focus:border-gx-accent"
            />
            <p className="text-[10px] text-gx-muted mt-0.5">
              Supports GitHub blob/tree URLs, raw URLs, or <code>owner/repo/path</code> shorthand.
            </p>
          </div>
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 px-4 py-3 border-t border-gx-border">
          <button
            className="rounded-md px-3 py-1.5 text-xs font-medium text-gx-muted hover:text-gx-text transition-colors"
            onClick={onClose}
          >
            Cancel
          </button>
          <button
            className="rounded-md bg-gx-accent px-3 py-1.5 text-xs font-medium text-white hover:opacity-90 transition-opacity disabled:opacity-50"
            onClick={handleInstall}
            disabled={installing}
          >
            {installing ? "Installing..." : "Install"}
          </button>
        </div>
      </div>
    </div>
  );
}
