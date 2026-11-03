import { useEffect, useState } from "react";
import { Code2, Eye, Loader2 } from "lucide-react";
import { Streamdown } from "streamdown";
import "streamdown/styles.css";
import { rpc } from "../../ipc";
import { CodeEditor } from "../CodeEditor";
import { useSetting } from "../../settings";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
}

/**
 * Markdown preview using streamdown (https://streamdown.ai).
 * Supports GFM, syntax-highlighted code blocks, tables, and more.
 * Toggle between rendered view and raw source (Monaco editor).
 */
export function MarkdownView({ repoId, path, rev }: Props) {
  const [text, setText] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [mode, setMode] = useState<"rendered" | "source">("rendered");
  const maxKib = useSetting("maxPreviewKib");

  useEffect(() => {
    let cancelled = false;
    setText(null);
    setErr(null);
    rpc
      .readText(repoId, path, {
        rev: rev ?? undefined,
        max_bytes: Math.max(maxKib * 1024, 64 * 1024),
      })
      .then((r) => {
        if (cancelled) return;
        if (r.truncated) setErr(r.reason ?? "file too large");
        else setText(r.text ?? "");
      })
      .catch((e: Error) => { if (!cancelled) setErr(e.message); });
    return () => { cancelled = true; };
  }, [repoId, path, rev, maxKib]);

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center justify-between border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
        <span>Markdown</span>
        <div className="flex gap-1">
          <button
            type="button"
            className="btn text-xs"
            onClick={() => setMode("rendered")}
            aria-pressed={mode === "rendered"}
          >
            <Eye size={12} /> Rendered
          </button>
          <button
            type="button"
            className="btn text-xs"
            onClick={() => setMode("source")}
            aria-pressed={mode === "source"}
          >
            <Code2 size={12} /> Source
          </button>
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-hidden bg-gx-bg-sub">
        {err && (
          <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{err}</div>
        )}
        {text == null && !err && (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
            <Loader2 size={14} className="animate-spin" /> Loading…
          </div>
        )}
        {text != null && mode === "rendered" && (
          <div className="h-full overflow-auto p-6">
            <Streamdown
              shikiTheme={["github-light", "github-dark"]}
            >
              {text}
            </Streamdown>
          </div>
        )}
        {text != null && mode === "source" && <CodeEditor path={path} value={text} />}
      </div>
    </div>
  );
}
