import { Button } from "@/components/ui/button";
import { useEffect, useState } from "react";
import { Binary, ChevronLeft, ChevronRight, Download, Loader2 } from "lucide-react";
import { rpc, isCancelledError, type CapturedFile, type FileChunkPage } from "../ipc";
import { useStore, type OpenRepo } from "../state";
import { formatBytes } from "../lib/format";
import { publishToast } from "../lib/toasts";

const PAGE_SIZE = 64;
const REPRESENTATION = {
  working_file: "Working file",
  git_blob: "Git blob",
  pointer: "Pointer in Git or working tree",
  remote_content: "Remote content",
};

export function FileInspector({ repo, path, rev }: { repo: OpenRepo; path: string; rev: string | null }) {
  const [file, setFile] = useState<CapturedFile | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const nativeSessionId = repo.native_session_id;

  useEffect(() => {
    setFile(null);
    setError(null);
    if (!nativeSessionId) return;
    const abort = new AbortController();
    let captured: CapturedFile | undefined;
    const close = (file: CapturedFile) => rpc.closeFile(repo.repo_id, file.file_id, nativeSessionId).catch((error: unknown) => {
      publishToast({ kind: "error", title: "Could not close file reader", body: error instanceof Error ? error.message : String(error) });
    });
    void rpc.openFile(repo.repo_id, path, { native_session_id: nativeSessionId, rev: rev ?? undefined, signal: abort.signal }).then(async (file) => {
      if (abort.signal.aborted) await close(file);
      else { captured = file; setFile(file); }
    }).catch((error: unknown) => {
      if (!abort.signal.aborted) setError(error instanceof Error ? error.message : String(error));
    });
    return () => {
      abort.abort();
      if (captured) void close(captured);
    };
  }, [repo.repo_id, path, rev, retry, nativeSessionId]);

  return <section aria-label="File details" className="h-full overflow-auto bg-background p-4 text-sm">
    <div className="mx-auto max-w-4xl space-y-4 rounded border border-border bg-card p-4">
      <h2 className="flex items-center gap-2 font-semibold"><Binary size={16} className="text-primary" />File details</h2>
      {!nativeSessionId ? <p role="status" className="text-muted-foreground">Waiting for repository recovery…</p> : error ? <div className="space-y-2"><p role="alert" className="break-words text-destructive">{error}</p><Button variant="outline" type="button" className="text-xs" onClick={() => setRetry(retry + 1)}>Retry opening file</Button></div> : !file ? <p role="status" className="flex items-center gap-2 text-muted-foreground"><Loader2 size={14} className="animate-spin" />Opening file…</p> : <>
        <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-5 gap-y-2 text-xs">
          <dt className="text-muted-foreground">Path</dt><dd className="break-all font-mono">{path}</dd>
          <dt className="text-muted-foreground">View</dt><dd>{rev === null ? "Working tree" : rev === "" ? "Git index" : rev}</dd>
          <dt className="text-muted-foreground">Logical size</dt><dd className="tabular-nums" title={`${file.size.toLocaleString()} bytes`}>{formatBytes(file.size)}</dd>
          <dt className="text-muted-foreground">Content</dt><dd>{file.kind === "crab" ? "Crab chunks" : file.kind === "lfs" ? "Git LFS" : "File bytes"}</dd>
          <dt className="text-muted-foreground">Representation</dt><dd>{REPRESENTATION[file.representation]}</dd>
          {file.content_hash && <><dt className="text-muted-foreground">{file.kind === "lfs" ? "SHA-256" : "BLAKE3"}</dt><dd className="break-all font-mono">{file.content_hash}</dd></>}
          {file.git_oid && <><dt className="text-muted-foreground">Git object</dt><dd className="break-all font-mono">{file.git_oid}</dd></>}
        </dl>
        {file.can_materialize && <MaterializeFile key={file.file_id} repoId={repo.repo_id} file={file} nativeSessionId={nativeSessionId} available={repo.capabilities?.canMaterialize ?? false} />}
        {file.kind === "crab" ? <ChunkDetails key={file.file_id} repoId={repo.repo_id} nativeSessionId={nativeSessionId} file={file} onReopen={() => setRetry(retry + 1)} /> : file.kind === "lfs" ? <p className="text-xs text-muted-foreground">Git LFS stores this file as one SHA-256 object. It has no Crab chunk recipe.</p> : null}
      </>}
    </div>
  </section>;
}

function ChunkDetails({ repoId, nativeSessionId, file, onReopen }: { repoId: string; nativeSessionId: string; file: CapturedFile; onReopen: () => void }) {
  const [expanded, setExpanded] = useState(false);
  const [start, setStart] = useState(0);
  const [page, setPage] = useState<FileChunkPage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    if (!expanded) return;
    const abort = new AbortController();
    setPage(null);
    setError(null);
    void rpc.fileChunks(repoId, file.file_id, nativeSessionId, start, PAGE_SIZE, abort.signal).then((page) => {
      if (!abort.signal.aborted) setPage(page);
    }).catch((error: unknown) => {
      if (!abort.signal.aborted) setError(error instanceof Error ? error.message : String(error));
    });
    return () => abort.abort();
  }, [expanded, repoId, file.file_id, nativeSessionId, start, retry]);

  return <div className="space-y-3 border-t border-border pt-3 text-xs">
    <Button variant="outline" type="button" className="text-xs" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>{expanded ? "Hide chunks" : "Inspect chunks"}</Button>
    {expanded && <>
      {error ? <div className="space-y-2"><p role="alert" className="break-words text-destructive">{error}</p><Button variant="outline" type="button" className="text-xs" onClick={() => setRetry(retry + 1)}>Retry chunk inspection</Button><Button variant="outline" type="button" className="ml-2 text-xs" onClick={onReopen}>Reopen file</Button></div> : !page ? <p role="status" className="flex items-center gap-2 text-muted-foreground"><Loader2 size={14} className="animate-spin" />Reading chunk metadata…</p> : <>
        <p className="tabular-nums">{page.chunk_count.toLocaleString()} chunk occurrences · {page.xorb_count === null ? "Published xorb count unavailable" : `${page.xorb_count.toLocaleString()} xorbs`}</p>
        <p className="text-muted-foreground">Ordered metadata for this captured file. Repeated hashes appear at each byte range. Placement is shown when published.</p>
        <div role="region" aria-label="Chunk metadata rows" tabIndex={0} className="max-h-64 overflow-auto rounded border border-border">
          <table aria-label="File chunks" className="w-full text-left text-[11px] tabular-nums">
            <thead className="sticky top-0 bg-card text-muted-foreground"><tr>{["Chunk", "Byte offset", "Bytes", "Hash", "Xorb placement"].map((label) => <th key={label} scope="col" className="px-2 py-2 font-medium">{label}</th>)}</tr></thead>
            <tbody>{page.chunks.map((chunk, index) => <tr key={start + index} className="border-t border-border">
              <td className="px-2 py-2">{(start + index + 1).toLocaleString()}</td><td className="px-2">{chunk.offset.toLocaleString()}</td><td className="px-2">{chunk.length.toLocaleString()}</td>
              <td className="max-w-44 break-all px-2 font-mono">{chunk.hash}</td>
              <td className="max-w-44 break-all px-2 font-mono">{chunk.xorb_hash ? `${chunk.xorb_hash} · ${chunk.xorb_chunk_index}` : "Unknown"}</td>
            </tr>)}</tbody>
          </table>
        </div>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <span aria-live="polite">{page.chunk_count === 0 ? "No chunks" : `Chunks ${(start + 1).toLocaleString()}–${(start + page.chunks.length).toLocaleString()} of ${page.chunk_count.toLocaleString()}`}</span>
          <div className="flex gap-2"><Button variant="outline" type="button" className="text-xs" aria-label="Previous chunks" disabled={start === 0} onClick={() => setStart(Math.max(0, start - PAGE_SIZE))}><ChevronLeft size={12} />Previous</Button><Button variant="outline" type="button" className="text-xs" aria-label="Next chunks" disabled={page.next === null} onClick={() => { if (page.next !== null) setStart(page.next); }}>Next<ChevronRight size={12} /></Button></div>
        </div>
      </>}
    </>}
  </div>;
}

function MaterializeFile({ repoId, file, nativeSessionId, available }: { repoId: string; file: CapturedFile; nativeSessionId: string; available: boolean }) {
  const { runFileTransfer } = useStore();
  const [pending, setPending] = useState(false);
  const [materialized, setMaterialized] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  async function hydrate() {
    if (!available) return;
    setPending(true);
    setMessage(null);
    setError(null);
    try {
      const result = await runFileTransfer(repoId, { fileId: file.file_id }, nativeSessionId);
      setMaterialized(true);
      setMessage(`${formatBytes(result.written_bytes)} materialized on disk. Reopen details to inspect the working copy.`);
    } catch (error) {
      if (isCancelledError(error)) setMessage("Materialization cancelled. The working pointer was preserved.");
      else setError(error instanceof Error ? error.message : String(error));
    } finally { setPending(false); }
  }
  return <div className="space-y-2 border-t border-border pt-3 text-xs">
    <Button variant="outline" type="button" className="text-xs" disabled={pending || materialized || !available} onClick={() => void hydrate()}>{pending ? <Loader2 size={12} className="animate-spin" /> : <Download size={12} />}{pending ? "Materializing file…" : "Hydrate to disk"}</Button>
    {pending && <p className="text-muted-foreground">Progress and cancellation are available in Activity. You can continue browsing.</p>}
    {message && <p role="status" className="text-muted-foreground">{message}</p>}
    {error && <p role="alert" className="break-words text-destructive">{error}</p>}
  </div>;
}
