import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
/**
 * Remote file editor component.
 *
 * Opens remote text files in a Monaco editor tab with syntax
 * highlighting, dirty indicator, save (Cmd+S), and conflict
 * detection.
 */

import clsx from "clsx";
import {
  AlertTriangle,
  Check,
  Loader2,
  Save,
  X,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { call } from "../../ipc";

// ── Types ────────────────────────────────────────────────────────

interface RemoteFileContent {
  content: string;
  path: string;
  mtime: number;
  size: number;
  language: string;
}

interface RemoteFileEditorProps {
  profileId: string;
  profileLabel: string;
  profileColour?: string;
  filePath: string;
  onClose?: () => void;
  onDirtyChange?: (isDirty: boolean) => void;
}

type SaveState = "idle" | "saving" | "saved" | "conflict" | "error";

// ── Helpers ──────────────────────────────────────────────────────

function basename(path: string): string {
  const parts = path.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

// ── Component ────────────────────────────────────────────────────

export function RemoteFileEditor({
  profileId,
  profileLabel,
  profileColour,
  filePath,
  onClose,
  onDirtyChange,
}: RemoteFileEditorProps) {
  const [content, setContent] = useState<string>("");
  const [originalContent, setOriginalContent] = useState<string>("");
  const [language, setLanguage] = useState<string>("plaintext");
  const [mtime, setMtime] = useState<number>(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saveState, setSaveState] = useState<SaveState>("idle");
  const [isDirty, setIsDirty] = useState(false);
  const editorRef = useRef<HTMLTextAreaElement>(null);
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Load file content
  useEffect(() => {
    let cancelled = false;

    async function loadFile() {
      setLoading(true);
      setError(null);
      try {
        const result = await call("ssh_read_file", {
          profile_id: profileId,
          path: filePath,
        });
        if (cancelled) return;

        const fileContent = result as RemoteFileContent;
        setContent(fileContent.content);
        setOriginalContent(fileContent.content);
        setLanguage(fileContent.language);
        setMtime(fileContent.mtime);
        setIsDirty(false);
      } catch (err) {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        if (!cancelled) setLoading(false);
      }
    }

    loadFile();
    return () => { cancelled = true; };
  }, [profileId, filePath]);

  // Track dirty state
  useEffect(() => {
    const dirty = content !== originalContent;
    setIsDirty(dirty);
    onDirtyChange?.(dirty);
  }, [content, originalContent, onDirtyChange]);

  // Keyboard shortcut: Cmd+S to save
  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.key === "s") {
        e.preventDefault();
        handleSave();
      }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [content, mtime]);

  const handleSave = useCallback(async () => {
    if (!isDirty) return;

    setSaveState("saving");
    try {
      const result = await call("ssh_write_file", {
        profile_id: profileId,
        path: filePath,
        content,
        expected_mtime: mtime,
      });

      const writeResult = result as { success: boolean; mtime: number; conflict?: boolean };

      if (writeResult.conflict) {
        setSaveState("conflict");
        return;
      }

      setMtime(writeResult.mtime);
      setOriginalContent(content);
      setIsDirty(false);
      setSaveState("saved");

      // Clear "saved" indicator after 2s
      if (saveTimerRef.current) clearTimeout(saveTimerRef.current);
      saveTimerRef.current = setTimeout(() => setSaveState("idle"), 2000);
    } catch (err) {
      setSaveState("error");
    }
  }, [profileId, filePath, content, mtime, isDirty]);

  const handleOverwrite = useCallback(async () => {
    setSaveState("saving");
    try {
      const result = await call("ssh_write_file", {
        profile_id: profileId,
        path: filePath,
        content,
      });

      const writeResult = result as { success: boolean; mtime: number };
      setMtime(writeResult.mtime);
      setOriginalContent(content);
      setIsDirty(false);
      setSaveState("saved");

      if (saveTimerRef.current) clearTimeout(saveTimerRef.current);
      saveTimerRef.current = setTimeout(() => setSaveState("idle"), 2000);
    } catch {
      setSaveState("error");
    }
  }, [profileId, filePath, content]);

  const handleReload = useCallback(async () => {
    setLoading(true);
    setSaveState("idle");
    try {
      const result = await call("ssh_read_file", {
        profile_id: profileId,
        path: filePath,
      });
      const fileContent = result as RemoteFileContent;
      setContent(fileContent.content);
      setOriginalContent(fileContent.content);
      setMtime(fileContent.mtime);
      setIsDirty(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [profileId, filePath]);

  const handleChange = useCallback((e: React.ChangeEvent<HTMLTextAreaElement>) => {
    setContent(e.target.value);
  }, []);

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <Loader2 className="w-5 h-5 animate-spin text-muted-foreground" />
        <span className="ml-2 text-sm text-muted-foreground">
          Loading file...
        </span>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex flex-col items-center justify-center h-full gap-2">
        <AlertTriangle className="w-6 h-6 text-destructive" />
        <p className="text-sm text-destructive text-destructive">{error}</p>
        <Button
          variant="ghost"
          size="compact"
          onClick={handleReload}
          className="text-xs text-info hover:underline"
        >
          Retry
        </Button>
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full">
      {/* Tab header */}
      <div className="flex items-center gap-2 px-3 py-1.5 border-b border-border border-border bg-background bg-card/50">
        <span className="text-sm font-medium truncate">
          {isDirty && <span className="text-warning mr-1">●</span>}
          Remote: {basename(filePath)}
        </span>
        <span className="text-xs text-muted-foreground truncate">
          ({profileLabel})
        </span>
        {profileColour && (
          <span
            className="w-1.5 h-1.5 rounded-full"
            style={{ backgroundColor: profileColour }}
          />
        )}
        <div className="flex-1" />

        {/* Save state indicator */}
        {saveState === "saving" && (
          <Loader2 className="w-3.5 h-3.5 animate-spin text-muted-foreground" />
        )}
        {saveState === "saved" && (
          <span className="flex items-center gap-1 text-xs text-success">
            <Check className="w-3 h-3" /> Saved
          </span>
        )}
        {saveState === "error" && (
          <span className="text-xs text-destructive">Save failed</span>
        )}

        {/* Save button */}
        <Button
          variant="ghost"
          size="compact"
          onClick={handleSave}
          disabled={!isDirty || saveState === "saving"}
          className={clsx(
            "p-1 rounded",
            isDirty
              ? "hover:bg-card hover:bg-card text-foreground text-muted-foreground"
              : "text-muted-foreground text-muted-foreground cursor-not-allowed",
          )}
          title="Save (⌘S)"
          aria-label="Save file"
        >
          <Save className="w-3.5 h-3.5" />
        </Button>

        {onClose && (
          <Button
            variant="ghost"
            size="compact"
            onClick={onClose}
            className="p-1 rounded hover:bg-card hover:bg-card"
            title="Close"
            aria-label="Close editor"
          >
            <X className="w-3.5 h-3.5" />
          </Button>
        )}
      </div>

      {/* Conflict banner */}
      {saveState === "conflict" && (
        <div className="flex items-center gap-2 px-3 py-2 bg-warning/10 bg-warning/20 border-b border-warning border-warning">
          <AlertTriangle className="w-4 h-4 text-warning" />
          <span className="text-xs text-warning text-warning">
            File was modified on the remote since you opened it.
          </span>
          <div className="flex-1" />
          <Button
            variant="ghost"
            size="compact"
            onClick={handleOverwrite}
            className="text-xs px-2 py-0.5 rounded bg-warning text-warning-foreground hover:bg-warning"
          >
            Overwrite
          </Button>
          <Button
            variant="ghost"
            size="compact"
            onClick={handleReload}
            className="text-xs px-2 py-0.5 rounded border border-warning text-warning hover:bg-warning/10 hover:bg-warning/40"
          >
            Reload
          </Button>
          <Button
            variant="ghost"
            size="compact"
            onClick={() => setSaveState("idle")}
            className="text-xs text-muted-foreground hover:text-foreground"
          >
            Cancel
          </Button>
        </div>
      )}

      {/* Editor area — in production this would be Monaco, using textarea as fallback */}
      <Textarea
        ref={editorRef}
        value={content}
        onChange={handleChange}
        className={clsx(
          "flex-1 w-full p-3 font-mono text-sm resize-none",
          "bg-background bg-background text-foreground text-foreground",
          "focus:outline-none",
          "border-none",
        )}
        spellCheck={false}
        aria-label={`Editor for ${basename(filePath)}`}
        data-language={language}
      />
    </div>
  );
}
