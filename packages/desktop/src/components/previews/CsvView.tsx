import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Download, Loader2 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { rpc } from "../../ipc";
import { useSetting } from "../../settings";

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
  sep: "," | "\t";
}

// Hand-rolled CSV/TSV parser: small, no-dep, handles quoted cells
// with embedded separators and newlines. For previews up to a few
// hundred thousand rows this is dramatically faster than shipping a
// parser library, and it leaves the bundle lean.
function parseDelimited(text: string, sep: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = "";
  let i = 0;
  let inQuotes = false;
  while (i < text.length) {
    const c = text[i];
    if (inQuotes) {
      if (c === '"') {
        if (text[i + 1] === '"') {
          field += '"';
          i += 2;
        } else {
          inQuotes = false;
          i++;
        }
      } else {
        field += c;
        i++;
      }
    } else {
      if (c === '"') {
        inQuotes = true;
        i++;
      } else if (c === sep) {
        row.push(field);
        field = "";
        i++;
      } else if (c === "\r") {
        // Normalise \r\n → row boundary.
        i++;
      } else if (c === "\n") {
        row.push(field);
        rows.push(row);
        row = [];
        field = "";
        i++;
      } else {
        field += c;
        i++;
      }
    }
  }
  if (field.length > 0 || row.length > 0) {
    row.push(field);
    rows.push(row);
  }
  return rows;
}

const ROWS_PER_PAGE = 200;

export function CsvView({ repoId, path, rev, sep }: Props) {
  const [text, setText] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [page, setPage] = useState(0);
  const [hasHeader, setHasHeader] = useState(true);
  const maxKib = useSetting("maxPreviewKib");

  useEffect(() => {
    let cancelled = false;
    setText(null);
    setErr(null);
    setPage(0);
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

  const rows = useMemo(() => (text ? parseDelimited(text, sep) : []), [text, sep]);
  const headers: string[] = useMemo(() => {
    if (!hasHeader || rows.length === 0) {
      // Synthesise column labels when no header is present.
      const cols = rows[0]?.length ?? 0;
      return Array.from({ length: cols }, (_, i) => `col${i + 1}`);
    }
    return rows[0];
  }, [rows, hasHeader]);
  const body = useMemo(() => (hasHeader ? rows.slice(1) : rows), [rows, hasHeader]);

  const totalPages = Math.max(1, Math.ceil(body.length / ROWS_PER_PAGE));
  const pageRows = body.slice(page * ROWS_PER_PAGE, (page + 1) * ROWS_PER_PAGE);

  if (err) {
    return (
      <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
        {err}
      </div>
    );
  }
  if (text == null) {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2 size={14} className="animate-spin" /> Parsing {path}…
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      <div className="flex shrink-0 items-center justify-between gap-3 border-b border-border bg-card px-3 py-1.5 text-[11px] text-muted-foreground">
        <div className="flex items-center gap-3">
          <span>{sep === "," ? "CSV" : "TSV"}</span>
          <span>
            · {body.length.toLocaleString()} rows · {headers.length} cols
          </span>
          <label className="flex items-center gap-1">
            <Checkbox
              checked={hasHeader}
              onCheckedChange={(checked) => setHasHeader(checked === true)}
            />
            Header row
          </label>
        </div>
        <div className="flex items-center gap-2">
          <Button variant="outline"
            type="button"
            className="text-xs"
            disabled={page === 0}
            onClick={() => setPage((p) => Math.max(0, p - 1))}
          >
            Prev
          </Button>
          <span>
            Page {page + 1} / {totalPages}
          </span>
          <Button variant="outline"
            type="button"
            className="text-xs"
            disabled={page + 1 >= totalPages}
            onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
          >
            Next
          </Button>
          <Button variant="outline"
            type="button"
            className="text-xs"
            onClick={() => {
              const blob = new Blob([text], {
                type: sep === "," ? "text/csv" : "text/tab-separated-values",
              });
              const a = document.createElement("a");
              a.href = URL.createObjectURL(blob);
              a.download = path.split("/").pop() ?? "data";
              a.click();
              URL.revokeObjectURL(a.href);
            }}
            title="Save a copy of the preview to disk"
          >
            <Download size={12} /> Export
          </Button>
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-auto bg-background">
        <table className="w-full border-collapse font-mono text-[12px]">
          <thead className="sticky top-0 bg-card">
            <tr>
              <th className="sticky left-0 border-b border-border bg-card px-2 py-1 text-right text-muted-foreground">
                #
              </th>
              {headers.map((h, i) => (
                <th
                  key={i}
                  className="border-b border-border px-2 py-1 text-left font-semibold text-foreground"
                  title={h}
                >
                  {h || `col${i + 1}`}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {pageRows.map((row, ri) => (
              <tr key={ri} className="odd:bg-card">
                <td className="sticky left-0 bg-inherit px-2 py-1 text-right text-muted-foreground">
                  {page * ROWS_PER_PAGE + ri + 1}
                </td>
                {headers.map((_, ci) => (
                  <td key={ci} className="border-b border-border/40 px-2 py-1 align-top text-foreground">
                    {row[ci] ?? ""}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
