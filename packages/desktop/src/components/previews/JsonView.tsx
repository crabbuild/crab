import { useEffect, useMemo, useState } from "react";
import clsx from "clsx";
import { ChevronDown, ChevronRight, Code2 } from "lucide-react";
import { rpc } from "../../ipc";
import { CodeEditor } from "../CodeEditor";
import { useSetting } from "../../settings";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
  /** When true, parse line-by-line (JSONL). */
  jsonl?: boolean;
}

// JSON/JSONL view: a collapsible tree for structured browsing plus a
// source toggle for pasteable raw output. For JSONL we show at most
// the first 500 lines to keep the tree responsive.
export function JsonView({ repoId, path, rev, jsonl }: Props) {
  const [text, setText] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [mode, setMode] = useState<"tree" | "source">("tree");
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
      .catch((e: Error) => {
        if (!cancelled) setErr(e.message);
      });
    return () => {
      cancelled = true;
    };
  }, [repoId, path, rev, maxKib]);

  const parsed = useMemo<{ ok: unknown; err?: never } | { ok?: never; err: string }>(() => {
    if (text == null) return { ok: null };
    try {
      if (jsonl) {
        const lines = text.split("\n").filter((l) => l.trim().length > 0).slice(0, 500);
        return { ok: lines.map((l) => JSON.parse(l)) };
      }
      return { ok: JSON.parse(text) };
    } catch (e) {
      return { err: e instanceof Error ? e.message : String(e) };
    }
  }, [text, jsonl]);

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center justify-between border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
        <span>{jsonl ? "JSONL (first 500 lines)" : "JSON"}</span>
        <div className="flex gap-1">
          <button type="button" className="btn text-xs" onClick={() => setMode("tree")}>
            Tree
          </button>
          <button type="button" className="btn text-xs" onClick={() => setMode("source")}>
            <Code2 size={12} /> Source
          </button>
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-auto bg-gx-bg-sub">
        {err && (
          <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
            {err}
          </div>
        )}
        {text != null && mode === "source" && <CodeEditor path={path} value={text} />}
        {text != null && mode === "tree" && parsed.err && (
          <div className="m-4 rounded border border-gx-warn/40 bg-gx-warn/10 p-3 text-xs text-gx-warn">
            Could not parse JSON: {parsed.err}
          </div>
        )}
        {text != null && mode === "tree" && parsed.ok !== undefined && (
          <div className="p-4 font-mono text-[12.5px]">
            <JsonNode value={parsed.ok} label={null} depth={0} />
          </div>
        )}
      </div>
    </div>
  );
}

// Recursive node. Arrays and objects render a chevron + count
// header; primitives render inline.
function JsonNode({ value, label, depth }: { value: unknown; label: string | null; depth: number }) {
  const [open, setOpen] = useState(depth < 2);
  if (value === null) return <Leaf label={label}>null</Leaf>;
  if (typeof value === "boolean" || typeof value === "number")
    return <Leaf label={label}>{String(value)}</Leaf>;
  if (typeof value === "string")
    return (
      <Leaf label={label}>
        <span className="text-[#9ece6a]">"{value}"</span>
      </Leaf>
    );
  if (Array.isArray(value)) {
    return (
      <div>
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          className="flex items-center gap-1 text-gx-text hover:text-gx-accent"
        >
          {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          {label != null && <span className="text-[#7aa2f7]">"{label}"</span>}
          {label != null && <span className="text-gx-muted">:</span>}
          <span className="text-gx-muted">[{value.length}]</span>
        </button>
        {open && (
          <div className="ml-4 border-l border-gx-border/60 pl-2">
            {value.map((v, i) => (
              <JsonNode key={i} value={v} label={String(i)} depth={depth + 1} />
            ))}
          </div>
        )}
      </div>
    );
  }
  if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>);
    return (
      <div>
        <button
          type="button"
          onClick={() => setOpen((o) => !o)}
          className="flex items-center gap-1 text-gx-text hover:text-gx-accent"
        >
          {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          {label != null && <span className="text-[#7aa2f7]">"{label}"</span>}
          {label != null && <span className="text-gx-muted">:</span>}
          <span className="text-gx-muted">{`{${entries.length}}`}</span>
        </button>
        {open && (
          <div className="ml-4 border-l border-gx-border/60 pl-2">
            {entries.map(([k, v]) => (
              <JsonNode key={k} value={v} label={k} depth={depth + 1} />
            ))}
          </div>
        )}
      </div>
    );
  }
  return <Leaf label={label}>{String(value)}</Leaf>;
}

function Leaf({ label, children }: { label: string | null; children: React.ReactNode }) {
  return (
    <div className={clsx("flex items-baseline gap-1 py-[1px]")}>
      {label != null && <span className="text-[#7aa2f7]">"{label}"</span>}
      {label != null && <span className="text-gx-muted">:</span>}
      <span>{children}</span>
    </div>
  );
}
