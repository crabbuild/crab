import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import { Streamdown } from "streamdown";
import "streamdown/styles.css";
import { rpc } from "../../ipc";
import { useSetting } from "../../settings";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
}

/**
 * Markdown preview using streamdown (https://streamdown.ai).
 * Supports GFM, syntax-highlighted code blocks, tables, and more.
 */
export function MarkdownView({ repoId, path, rev }: Props) {
  const [text, setText] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
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
    <div className="h-full min-h-0 overflow-hidden bg-gx-bg-sub">
      {err && (
        <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">{err}</div>
      )}
      {text == null && !err && (
        <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
          <Loader2 size={14} className="animate-spin" /> Loading…
        </div>
      )}
      {text != null && (
        <div className="h-full overflow-auto p-6">
          <Streamdown
            shikiTheme={["github-light", "github-dark"]}
          >
            {text}
          </Streamdown>
        </div>
      )}
    </div>
  );
}
