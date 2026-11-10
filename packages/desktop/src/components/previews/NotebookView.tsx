import { useCallback, useEffect, useMemo, useRef, useState, Component, type ReactNode } from "react";
import { Streamdown } from "streamdown";
import { ChevronDown, ChevronRight, Code2, FileText, Loader2, Pause, Play, Power, RefreshCw, Terminal, Zap } from "lucide-react";
import { rpc } from "../../ipc";
import { useEffectiveTheme } from "../../hooks/useEffectiveTheme";

// Use a simple pre-formatted code block instead of Monaco to avoid
// TDZ/initialization crashes when rendering many cells.

interface JupyterOutput { id: string; msgType: string; content: Record<string, unknown>; parentMsgId: string; }
interface KernelSpec { name: string; displayName: string; language: string; }

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
  nativeSessionId: string | null;
}

// Jupyter IPC — access directly via window.crab to avoid any
// circular import / TDZ issues. The preload exposes these methods.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const w = typeof window !== "undefined" ? (window as any).crab ?? {} : {};
const jupyterIpc = {
  listKernels: (): Promise<KernelSpec[]> => w.jupyterListKernels?.() ?? Promise.resolve([]),
  start: (name: string, cwd: string, jupyterPath?: string) => w.jupyterStart?.(name, cwd, jupyterPath) ?? Promise.resolve({ id: null, status: "error", message: "not available" }),
  execute: (id: string, code: string) => w.jupyterExecute?.(id, code) ?? Promise.resolve({ error: "not available" }),
  interrupt: (id: string) => w.jupyterInterrupt?.(id) ?? Promise.resolve(),
  restart: (id: string) => w.jupyterRestart?.(id) ?? Promise.resolve(),
  shutdown: (id: string) => w.jupyterShutdown?.(id) ?? Promise.resolve(),
  onOutput: (listener: (evt: JupyterOutput) => void) => w.onJupyterOutput?.(listener) ?? (() => {}),
  onStatus: (listener: (evt: { id: string; status: string }) => void) => w.onJupyterStatus?.(listener) ?? (() => {}),
};

interface IpynbCell {
  cell_type: "code" | "markdown" | "raw";
  source: string | string[];
  outputs?: IpynbOutput[];
  execution_count?: number | null;
  metadata?: Record<string, unknown>;
}
interface IpynbOutput {
  output_type: string;
  name?: string;
  text?: string | string[];
  data?: Record<string, string | string[]>;
  ename?: string;
  evalue?: string;
  traceback?: string[];
}
interface Ipynb {
  cells: IpynbCell[];
  metadata?: {
    kernelspec?: { display_name?: string; language?: string };
    language_info?: { name?: string; version?: string };
  };
  nbformat?: number;
  nbformat_minor?: number;
}

export function NotebookView({ repoId, path, rev, nativeSessionId }: Props) {
  const [text, setText] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);

  const nb = useMemo<Ipynb | string>(() => {
    if (text == null) return "";
    try { return JSON.parse(text) as Ipynb; }
    catch (e) { return e instanceof Error ? e.message : String(e); }
  }, [text]);

  const lang = typeof nb !== "string" ? (nb.metadata?.language_info?.name ?? nb.metadata?.kernelspec?.language ?? "python") : "python";

  // ---- Kernel state (must be before any early returns) ----
  const [kernelId, setKernelId] = useState<string | null>(null);
  const [kernelStatus, setKernelStatus] = useState<string>("disconnected");
  const [kernelSpecs, setKernelSpecs] = useState<KernelSpec[]>([]);
  const [cellOutputs, setCellOutputs] = useState<Map<number, IpynbOutput[]>>(new Map());
  const [runningCell, setRunningCell] = useState<number | null>(null);
  const msgToCellRef = useRef<Map<string, number>>(new Map());
  const documentText = useRef<string | null>(null);

  useEffect(() => {
    const abort = new AbortController();
    setText(null); setErr(null);
    if (!nativeSessionId) return;
    void rpc.readText(repoId, path, { rev: rev ?? undefined, max_bytes: 20 * 1024 * 1024, native_session_id: nativeSessionId, signal: abort.signal }).then(result => {
      if (abort.signal.aborted) return;
      if (result.truncated || result.text === null) { setErr(result.reason ?? "The notebook was not read completely."); return; }
      if (documentText.current !== result.text) {
        // Kernel variables survive native repair; outputs belong to the
        // document that ran them and cannot attach to replacement cells.
        msgToCellRef.current.clear(); setCellOutputs(new Map()); setRunningCell(null);
      }
      documentText.current = result.text;
      setText(result.text);
    }).catch((error: Error) => { if (!abort.signal.aborted) setErr(error.message); });
    return () => { abort.abort(); };
  }, [repoId, path, rev, nativeSessionId, retry]);

  useEffect(() => {
    jupyterIpc.listKernels().then(setKernelSpecs).catch(() => {});
  }, []);

  useEffect(() => {
    const offOutput = jupyterIpc.onOutput((evt: JupyterOutput) => {
      if (evt.id !== kernelId) return;
      const cellIdx = msgToCellRef.current.get(evt.parentMsgId);
      if (cellIdx == null) return;
      const output = kernelOutputToIpynb(evt);
      if (!output) return;
      setCellOutputs((prev) => {
        const next = new Map(prev);
        const existing = next.get(cellIdx) ?? [];
        next.set(cellIdx, [...existing, output]);
        return next;
      });
      if (evt.msgType === "execute_reply") setRunningCell(null);
    });
    const offStatus = jupyterIpc.onStatus((evt) => {
      if (evt.id === kernelId) setKernelStatus(evt.status);
    });
    return () => { offOutput(); offStatus(); };
  }, [kernelId]);

  const [detectedJupyterPath, setDetectedJupyterPath] = useState<string | null>(null);

  const connectKernel = useCallback(async (specName?: string, jupyterPath?: string) => {
    const name = specName ?? kernelSpecs.find(s => s.language === lang)?.name ?? "python3";
    setKernelStatus("starting");
    // If no path provided, run a check first to detect it.
    let effectivePath = jupyterPath ?? detectedJupyterPath;
    if (!effectivePath) {
      try {
        const check = await (w.jupyterCheck?.() as Promise<{ jupyterPath: string | null }>);
        if (check?.jupyterPath) {
          effectivePath = check.jupyterPath;
          setDetectedJupyterPath(effectivePath);
        }
      } catch { /* ignore */ }
    }
    if (jupyterPath) setDetectedJupyterPath(jupyterPath);
    const result = await jupyterIpc.start(name, "", effectivePath ?? undefined);
    if (result.id) { setKernelId(result.id); setKernelStatus("idle"); }
    else { setKernelStatus(`error: ${result.message ?? "failed"} (path: ${effectivePath ?? "none"})`); }
  }, [kernelSpecs, lang, detectedJupyterPath]);

  const disconnectKernel = useCallback(async () => {
    if (kernelId) { await jupyterIpc.shutdown(kernelId); setKernelId(null); setKernelStatus("disconnected"); }
  }, [kernelId]);

  const runCell = useCallback(async (cellIdx: number, code: string) => {
    if (!kernelId) return;
    setCellOutputs((prev) => { const next = new Map(prev); next.set(cellIdx, []); return next; });
    setRunningCell(cellIdx);
    const result = await jupyterIpc.execute(kernelId, code);
    if (result.msgId) msgToCellRef.current.set(result.msgId, cellIdx);
  }, [kernelId]);

  // ---- Early returns (AFTER all hooks) ----
  if (!nativeSessionId) return <p role="status" className="p-4 text-xs text-gx-muted">Waiting for repository recovery…</p>;
  if (err) return <div className="m-4 space-y-3 text-xs"><p role="alert" className="rounded border border-gx-error/40 bg-gx-error/10 p-3 text-gx-error">{err}</p><button type="button" className="btn" onClick={() => setRetry(retry + 1)}>Retry notebook</button></div>;
  if (text == null) return <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted"><Loader2 size={14} className="animate-spin" /> Reading notebook…</div>;
  if (typeof nb === "string") return <div className="m-4 rounded border border-gx-warn/40 bg-gx-warn/10 p-3 text-xs text-gx-warn">Could not parse notebook JSON: {nb}</div>;

  // These are safe after early returns since they don't use hooks.
  const kernel = nb.metadata?.kernelspec?.display_name ?? lang;
  const version = nb.metadata?.language_info?.version;
  const cells = nb.cells ?? (nb as any).worksheets?.[0]?.cells ?? [];

  // If the notebook has 0 cells, it might be a pointer that wasn't hydrated.
  const looksLikePointer = cells.length === 0 && text != null && (
    text.includes("crab_hash") || text.includes("oid sha256:") || text.length < 500
  );

  const runAllCells = async () => {
    if (!kernelId) return;
    for (let i = 0; i < cells.length; i++) {
      if (cells[i].cell_type === "code") {
        const source = Array.isArray(cells[i].source) ? (cells[i].source as string[]).join("") : cells[i].source as string;
        await runCell(i, source);
      }
    }
  };

  return (
    <NotebookErrorBoundary>
    <div className="h-full overflow-auto bg-gx-bg-sub">
      {/* Show hydrate prompt if notebook appears to be a pointer */}
      {looksLikePointer && (
        <div className="m-6 rounded-lg border border-gx-warn/40 bg-gx-warn/10 p-6 text-center">
          <div className="mb-2 text-lg font-semibold text-gx-text">Notebook not hydrated</div>
          <div className="mb-4 text-sm text-gx-muted">
            This file appears to be a Crab pointer. Run <code className="kbd">crab hydrate</code> to download the full notebook content.
          </div>
          <div className="text-[11px] text-gx-muted">
            File size: {text?.length ?? 0} bytes · Parsed cells: {cells.length}
          </div>
        </div>
      )}
      {/* Notebook header with kernel controls */}
      <div className="sticky top-0 z-10 flex items-center gap-3 border-b border-gx-border bg-gx-surface px-4 py-2 text-[11px] text-gx-muted">
        <FileText size={12} className="text-gx-accent" />
        <span className="font-medium text-gx-text">{path.split("/").pop()}</span>
        <span className="chip">{kernel}{version ? ` ${version}` : ""}</span>
        <span>{cells.length} cells</span>

        {/* Kernel controls */}
        <div className="ml-auto flex items-center gap-2">
          {/* Status indicator */}
          <span className={`flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px] ${
            kernelStatus === "idle" ? "bg-gx-success/20 text-gx-success" :
            kernelStatus === "busy" ? "bg-gx-warn/20 text-gx-warn" :
            kernelStatus === "disconnected" ? "bg-gx-muted/20 text-gx-muted" :
            "bg-gx-error/20 text-gx-error"
          }`}>
            <span className={`h-1.5 w-1.5 rounded-full ${
              kernelStatus === "idle" ? "bg-gx-success" :
              kernelStatus === "busy" ? "bg-gx-warn animate-pulse" :
              "bg-gx-muted"
            }`} />
            {kernelStatus}
          </span>

          {!kernelId ? (
            <KernelConnectButton
              status={kernelStatus}
              onConnect={connectKernel}
            />
          ) : (
            <>
              <button type="button" className="btn text-[10px]" onClick={() => void runAllCells()} disabled={kernelStatus === "busy"}>
                <Play size={10} /> Run All
              </button>
              <button type="button" className="btn text-[10px]" onClick={() => void jupyterIpc.interrupt(kernelId)} title="Interrupt" aria-label="Interrupt kernel">
                <Pause size={10} />
              </button>
              <button type="button" className="btn text-[10px]" onClick={() => void jupyterIpc.restart(kernelId)} title="Restart kernel" aria-label="Restart kernel">
                <RefreshCw size={10} />
              </button>
              <button type="button" className="btn text-[10px]" onClick={() => void disconnectKernel()} title="Disconnect" aria-label="Disconnect kernel">
                <Power size={10} />
              </button>
            </>
          )}
        </div>
      </div>

      <div className="mx-auto max-w-5xl space-y-1 p-4">
        {cells.map((cell, i) => (
          <CellView
            key={i}
            cell={cell}
            index={i}
            language={lang}
            kernelConnected={!!kernelId}
            isRunning={runningCell === i}
            liveOutputs={cellOutputs.get(i)}
            onRun={(code) => void runCell(i, code)}
          />
        ))}
      </div>
    </div>
    </NotebookErrorBoundary>
  );
}

function CellView({ cell, index, language, kernelConnected, isRunning, liveOutputs, onRun }: {
  cell: IpynbCell;
  index: number;
  language: string;
  kernelConnected: boolean;
  isRunning: boolean;
  liveOutputs?: IpynbOutput[];
  onRun: (code: string) => void;
}) {
  const [collapsed, setCollapsed] = useState(false);
  // nbformat 3 uses `input` for code cells; nbformat 4 uses `source`.
  const rawSource = (cell as any).source ?? (cell as any).input ?? "";
  const source = Array.isArray(rawSource) ? rawSource.join("") : (rawSource ?? "");
  const lineCount = source.split("\n").length;

  if (cell.cell_type === "markdown") {
    return (
      <div className="group rounded-lg border border-gx-border/60 bg-gx-surface transition hover:border-gx-border">
        <div className="flex items-center gap-2 px-3 py-1.5 text-[10px] text-gx-muted">
          <button type="button" onClick={() => setCollapsed(!collapsed)} className="hover:text-gx-text" aria-label={collapsed ? "Expand cell" : "Collapse cell"}>
            {collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
          </button>
          <FileText size={11} />
          <span>Markdown</span>
          <span className="ml-auto">[{index + 1}]</span>
        </div>
        {!collapsed && (
          <div className="border-t border-gx-border/40 px-4 py-3">
            <Streamdown shikiTheme={["github-light", "github-dark"]}>
              {source}
            </Streamdown>
          </div>
        )}
      </div>
    );
  }

  if (cell.cell_type === "code") {
    return (
      <div className="group rounded-lg border border-gx-border/60 bg-gx-surface transition hover:border-gx-border">
        {/* Cell header */}
        <div className="flex items-center gap-2 px-3 py-1.5 text-[10px] text-gx-muted">
          <button type="button" onClick={() => setCollapsed(!collapsed)} className="hover:text-gx-text" aria-label={collapsed ? "Expand cell" : "Collapse cell"}>
            {collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
          </button>
          <Code2 size={11} className="text-gx-accent" />
          <span className="font-mono">{language}</span>
          {/* Execution count badge */}
          <span className="flex items-center gap-1 rounded bg-gx-bg-sub px-1.5 py-[1px] font-mono text-[10px]">
            <Play size={8} className="text-gx-success" />
            {cell.execution_count ?? " "}
          </span>
          <span className="ml-auto flex items-center gap-2 text-[9px]">
            {kernelConnected && (
              <button
                type="button"
                className="flex items-center gap-1 rounded bg-gx-success/20 px-1.5 py-0.5 text-gx-success hover:bg-gx-success/30 disabled:opacity-50"
                onClick={() => onRun(source)}
                disabled={isRunning}
              >
                {isRunning ? <Loader2 size={9} className="animate-spin" /> : <Play size={9} />}
                Run
              </button>
            )}
            {lineCount} lines
          </span>
        </div>

        {!collapsed && (
          <>
            {/* Code block */}
            <div className="border-t border-gx-border/40 overflow-auto" style={{ maxHeight: 400 }}>
              <pre className="p-3 font-mono text-[12px] leading-5 text-gx-text whitespace-pre-wrap">
                <code>{source}</code>
              </pre>
            </div>

            {/* Outputs — show live outputs if available, otherwise saved */}
            {(() => {
              const outputs = liveOutputs && liveOutputs.length > 0 ? liveOutputs : cell.outputs;
              const hasOutputs = outputs && outputs.length > 0;
              if (!hasOutputs) return null;
              return (
                <div className="border-t border-gx-border/40 bg-gx-bg-sub p-3">
                  <div className="mb-1 flex items-center gap-1 text-[9px] uppercase tracking-wider text-gx-muted">
                    <Terminal size={9} /> Output
                    {liveOutputs && liveOutputs.length > 0 && <span className="text-gx-success">(live)</span>}
                  </div>
                  <div className="space-y-2">
                    {outputs!.map((o, i) => (
                      <OutputView key={i} output={o} />
                    ))}
                  </div>
                </div>
              );
            })()}
          </>
        )}
      </div>
    );
  }

  // Raw cell.
  if (cell.cell_type === "raw") {
    return (
      <div className="rounded-lg border border-gx-border/40 bg-gx-bg-sub p-3">
        <div className="mb-1 text-[10px] text-gx-muted">Raw [{index + 1}]</div>
        <pre className="font-mono text-[12px] text-gx-text">{source}</pre>
      </div>
    );
  }

  return null;
}

function OutputView({ output }: { output: IpynbOutput }) {
  // Error output (traceback).
  if (output.output_type === "error" && output.traceback) {
    const tb = output.traceback.join("\n");
    // Strip ANSI codes for display.
    const clean = tb.replace(/\x1b\[[0-9;]*m/g, "");
    return (
      <div className="rounded border border-gx-error/30 bg-gx-error/5 p-2">
        <div className="mb-1 font-mono text-[11px] font-bold text-gx-error">
          {output.ename}: {output.evalue}
        </div>
        <pre className="max-h-[200px] overflow-auto font-mono text-[11px] text-gx-error/80">{clean}</pre>
      </div>
    );
  }

  // Stream output (stdout/stderr).
  if (output.output_type === "stream") {
    const text = Array.isArray(output.text) ? output.text.join("") : output.text ?? "";
    const isErr = output.name === "stderr";
    return (
      <pre className={`max-h-[300px] overflow-auto font-mono text-[12px] ${isErr ? "text-gx-warn" : "text-gx-text"}`}>
        {text}
      </pre>
    );
  }

  // Display data / execute_result.
  const data = output.data;
  if (!data) {
    const text = Array.isArray(output.text) ? output.text.join("") : output.text;
    if (text) return <pre className="font-mono text-[12px] text-gx-text">{text}</pre>;
    return null;
  }

  // Priority: image > SVG > HTML > LaTeX > text.
  const png = joinData(data["image/png"]);
  const jpeg = joinData(data["image/jpeg"]);
  const svg = joinData(data["image/svg+xml"]);
  const html = joinData(data["text/html"]);
  const latex = joinData(data["text/latex"]);
  const plain = joinData(data["text/plain"]);

  return (
    <div className="space-y-2">
      {png && <img src={`data:image/png;base64,${png}`} alt="output" className="max-w-full rounded" />}
      {jpeg && !png && <img src={`data:image/jpeg;base64,${jpeg}`} alt="output" className="max-w-full rounded" />}
      {svg && !png && !jpeg && (
        <NotebookRichOutput kind="SVG" content={svg} />
      )}
      {html && !png && !jpeg && !svg && (
        <NotebookRichOutput kind="HTML" content={html} />
      )}
      {latex && !html && !png && !svg && (
        <div className="font-mono text-[12px] text-gx-text">{latex}</div>
      )}
      {plain && !png && !jpeg && !svg && !html && (
        <pre className="max-h-[300px] overflow-auto font-mono text-[12px] text-gx-text">{plain}</pre>
      )}
    </div>
  );
}

function NotebookRichOutput({ kind, content }: { kind: "HTML" | "SVG"; content: string }) {
  const theme = useEffectiveTheme();
  // Saved/live markup is data, not code for the privileged renderer. Keep
  // script and same-origin exemptions absent so it cannot reach window.crab.
  const source = `<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:; base-uri 'none'; form-action 'none'"><meta name="color-scheme" content="${theme}"><style>:root{color-scheme:${theme}}body{margin:0;color:CanvasText;background:Canvas;font:13px/1.5 system-ui;overflow-wrap:anywhere}img,svg{max-width:100%}svg{fill:currentColor}table{border-collapse:collapse}th,td{padding:.25rem .5rem;text-align:start}</style></head><body>${content}</body></html>`;
  return <iframe title={`Notebook ${kind} output`} sandbox="" referrerPolicy="no-referrer" srcDoc={source} className="h-48 min-h-24 w-full resize-y rounded border-0 bg-gx-bg-sub" />;
}

function joinData(d: string | string[] | undefined): string | null {
  if (!d) return null;
  return Array.isArray(d) ? d.join("") : d;
}

/**
 * Convert a Jupyter kernel output message to the IpynbOutput format
 * used by the cell renderer.
 */
function kernelOutputToIpynb(evt: JupyterOutput): IpynbOutput | null {
  const c = evt.content as Record<string, unknown>;
  switch (evt.msgType) {
    case "stream":
      return { output_type: "stream", name: c.name as string, text: c.text as string };
    case "execute_result":
    case "display_data":
      return { output_type: evt.msgType, data: c.data as Record<string, string | string[]> };
    case "error":
      return {
        output_type: "error",
        ename: c.ename as string,
        evalue: c.evalue as string,
        traceback: c.traceback as string[],
      };
    default:
      return null;
  }
}


/**
 * Kernel connection button with "not found" UX, auto-install, and
 * remote server connection support.
 */
function KernelConnectButton({ status, onConnect }: { status: string; onConnect: (specName?: string, jupyterPath?: string) => void }) {
  const [showPanel, setShowPanel] = useState(false);
  const [checking, setChecking] = useState(false);
  const [checkResult, setCheckResult] = useState<{ available: boolean; jupyterPath: string | null; pythonPath: string | null } | null>(null);
  const [installing, setInstalling] = useState(false);
  const [installResult, setInstallResult] = useState<string | null>(null);
  const [remoteUrl, setRemoteUrl] = useState("");

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const w = typeof window !== "undefined" ? (window as any).crab ?? {} : {};

  const checkJupyter = async () => {
    setChecking(true);
    try {
      const result = await w.jupyterCheck?.();
      setCheckResult(result);
    } catch { setCheckResult({ available: false, jupyterPath: null, pythonPath: null }); }
    finally { setChecking(false); }
  };

  const installJupyter = async () => {
    setInstalling(true);
    setInstallResult(null);
    try {
      const result = await w.jupyterInstall?.();
      if (result?.success) {
        setInstallResult("✓ Installed successfully! Click Connect to start.");
        setCheckResult({ available: true, jupyterPath: "jupyter", pythonPath: checkResult?.pythonPath ?? null });
      } else {
        setInstallResult(`✗ ${result?.output ?? "Installation failed"}`);
      }
    } catch (e) { setInstallResult(`✗ ${e instanceof Error ? e.message : String(e)}`); }
    finally { setInstalling(false); }
  };

  if (!showPanel) {
    return (
      <button
        type="button"
        className="btn btn-primary text-[10px]"
        onClick={() => { setShowPanel(true); void checkJupyter(); }}
        disabled={status === "starting"}
      >
        <Zap size={10} /> Connect Kernel
      </button>
    );
  }

  return (
    <div className="absolute right-4 top-10 z-20 w-80 rounded-lg border border-gx-border bg-gx-surface p-4 shadow-2xl">
      <div className="mb-3 flex items-center justify-between">
        <span className="text-[12px] font-semibold">Connect to Kernel</span>
        <button type="button" onClick={() => setShowPanel(false)} className="text-gx-muted hover:text-gx-text text-[11px]" aria-label="Close panel">✕</button>
      </div>

      {checking && <div className="text-[11px] text-gx-muted">Checking for Jupyter…</div>}

      {checkResult && checkResult.available && (
        <div className="space-y-2">
          <div className="flex items-start gap-2 text-[11px] text-gx-success">
            <span className="mt-0.5 h-2 w-2 shrink-0 rounded-full bg-gx-success" />
            <span className="min-w-0 break-all">Jupyter found at <code className="font-mono text-[10px]">{checkResult.jupyterPath}</code></span>
          </div>
          <button type="button" className="btn btn-primary w-full text-[11px]" onClick={() => { setShowPanel(false); onConnect(undefined, checkResult.jupyterPath ?? undefined); }}>
            <Play size={10} /> Start Local Kernel
          </button>
        </div>
      )}

      {checkResult && !checkResult.available && (
        <div className="space-y-3">
          <div className="flex items-center gap-2 text-[11px] text-gx-warn">
            <span className="h-2 w-2 rounded-full bg-gx-warn" /> Jupyter not found
          </div>
          {checkResult.pythonPath && (
            <div className="text-[10px] text-gx-muted">Python found at: {checkResult.pythonPath}</div>
          )}
          <button
            type="button"
            className="btn btn-primary w-full text-[11px]"
            onClick={() => void installJupyter()}
            disabled={installing}
          >
            {installing ? <><Loader2 size={10} className="animate-spin" /> Installing…</> : <><Zap size={10} /> Auto-install Jupyter + ipykernel</>}
          </button>
          {installResult && (
            <div className={`text-[10px] ${installResult.startsWith("✓") ? "text-gx-success" : "text-gx-error"}`}>
              {installResult}
            </div>
          )}
          <div className="text-[10px] text-gx-muted">
            Or install manually: <code className="kbd">pip install jupyter ipykernel</code>
          </div>
        </div>
      )}

      {/* Remote server connection */}
      <div className="mt-3 border-t border-gx-border pt-3">
        <div className="mb-1 text-[10px] uppercase tracking-wider text-gx-muted">Remote Server</div>
        <div className="flex gap-1">
          <input
            className="flex-1 rounded border border-gx-border bg-gx-bg-sub px-2 py-1 font-mono text-[10px] focus-visible:outline-none focus:border-gx-accent"
            placeholder="http://localhost:8888/?token=..."
            value={remoteUrl}
            onChange={(e) => setRemoteUrl(e.target.value)}
          />
          <button
            type="button"
            className="btn text-[10px]"
            disabled={!remoteUrl.trim()}
            onClick={() => { setShowPanel(false); /* TODO: connect to remote */ }}
          >
            Connect
          </button>
        </div>
        <div className="mt-1 text-[9px] text-gx-muted">
          Paste a Jupyter server URL to connect to a remote kernel.
        </div>
      </div>
    </div>
  );
}

// Error boundary to prevent notebook rendering crashes from killing the app.
class NotebookErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null };
  static getDerivedStateFromError(error: Error) { return { error }; }
  render() {
    if (this.state.error) {
      return (
        <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-4 text-xs text-gx-error">
          <div className="mb-2 font-semibold">Notebook rendering error</div>
          <pre className="whitespace-pre-wrap">{this.state.error.message}</pre>
          <button
            type="button"
            className="btn mt-3 text-xs"
            onClick={() => this.setState({ error: null })}
          >
            Retry
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
