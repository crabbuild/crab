import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { ChevronDown, ChevronRight, Code2, FileText, Loader2, Pause, Play, Power, RefreshCw, Terminal, Zap } from "lucide-react";
import {
  Component,
  useEffect,
  useMemo,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import { Streamdown } from "streamdown";
import type { JupyterRuntime, KernelSpec } from "../../../electron/jupyter-types";
import { useEffectiveTheme } from "../../hooks/useEffectiveTheme";
import { rpc } from "../../ipc";
import { notebookKernel, type NotebookOutput as IpynbOutput } from "../../lib/jupyter-ipc";

// Use a simple pre-formatted code block instead of Monaco to avoid
// TDZ/initialization crashes when rendering many cells.

interface Props {
  repoId: string;
  path: string;
  rev: string | null;
  nativeSessionId: string | null;
}

interface IpynbCell {
  cell_type: "code" | "markdown" | "raw";
  source: string | string[];
  outputs?: IpynbOutput[];
  execution_count?: number | null;
  metadata?: Record<string, unknown>;
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
  const kernelOwner = useMemo(() => notebookKernel(repoId, path, rev), [repoId, path, rev]);
  const kernelState = useSyncExternalStore(kernelOwner.subscribe, kernelOwner.snapshot, kernelOwner.snapshot);
  const { id: kernelId, status: kernelStatus, outputs: cellOutputs, running, counts } = kernelState;

  const nb = useMemo<Ipynb | string>(() => {
    if (text == null) return "";
    try { return JSON.parse(text) as Ipynb; }
    catch (e) { return e instanceof Error ? e.message : String(e); }
  }, [text]);

  const lang = typeof nb !== "string" ? (nb.metadata?.language_info?.name ?? nb.metadata?.kernelspec?.language ?? "python") : "python";

  useEffect(() => {
    const abort = new AbortController();
    setText(null); setErr(null);
    if (!nativeSessionId) return;
    void rpc.readText(repoId, path, { rev: rev ?? undefined, max_bytes: 20 * 1024 * 1024, native_session_id: nativeSessionId, signal: abort.signal }).then(result => {
      if (abort.signal.aborted) return;
      if (result.truncated || result.text === null) { setErr(result.reason ?? "The notebook was not read completely."); return; }
      kernelOwner.document(result.text);
      setText(result.text);
    }).catch((error: Error) => { if (!abort.signal.aborted) setErr(error.message); });
    return () => { abort.abort(); };
  }, [repoId, path, rev, nativeSessionId, retry, kernelOwner]);

  // ---- Early returns (AFTER all hooks) ----
  if (!nativeSessionId)
    return (
      <p role="status" className="p-4 text-xs text-muted-foreground">
        Waiting for repository recovery…
      </p>
    );
  if (err)
    return (
      <div className="m-4 space-y-3 text-xs">
        <p role="alert" className="rounded border border-destructive/40 bg-destructive/10 p-3 text-destructive">
          {err}
        </p>
        <Button variant="outline" type="button" onClick={() => setRetry(retry + 1)}>
          Retry notebook
        </Button>
      </div>
    );
  if (text == null)
    return (
      <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2 size={14} className="animate-spin" /> Reading notebook…
      </div>
    );
  if (typeof nb === "string")
    return (
      <div className="m-4 rounded border border-warning/40 bg-warning/10 p-3 text-xs text-warning">
        Could not parse notebook JSON: {nb}
      </div>
    );

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
        const source = Array.isArray(cells[i].source)
          ? (cells[i].source as string[]).join("")
          : (cells[i].source as string);
        if (!(await kernelOwner.run(i, source))) break;
      }
    }
  };

  return (
    <NotebookErrorBoundary>
      <div className="h-full overflow-auto bg-background">
        {/* Show hydrate prompt if notebook appears to be a pointer */}
        {looksLikePointer && (
          <div className="m-6 rounded-lg border border-warning/40 bg-warning/10 p-6 text-center">
            <div className="mb-2 text-lg font-semibold text-foreground">
              Notebook not hydrated
            </div>
            <div className="mb-4 text-sm text-muted-foreground">
              This file appears to be a Crab pointer. Run{" "}
              <code className="kbd">crab hydrate</code> to download the full
              notebook content.
            </div>
            <div className="text-[11px] text-muted-foreground">
              File size: {text?.length ?? 0} bytes · Parsed cells:{" "}
              {cells.length}
            </div>
          </div>
        )}
        {/* Notebook header with kernel controls */}
        <div className="sticky top-0 z-10 flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card px-4 py-2 text-[11px] text-muted-foreground">
          <FileText size={12} className="shrink-0 text-primary" />
          <span className="min-w-0 truncate font-medium text-foreground">
            {path.split("/").pop()}
          </span>
          <span className="chip shrink-0">
            {kernel}
            {version ? ` ${version}` : ""}
          </span>
          <span className="shrink-0">{cells.length} cells</span>

          {/* Kernel controls */}
          <div className="ml-auto flex shrink-0 items-center gap-2 whitespace-nowrap">
            {/* Kernel controls */}
            <span role="status" aria-label="Kernel status" className={`flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px] ${
            kernelStatus === "idle" ? "bg-success/20 text-success" :
            kernelStatus === "busy" ? "bg-warning/20 text-warning" :
            kernelStatus === "disconnected" ? "bg-muted-foreground/20 text-muted-foreground" :
            "bg-destructive/20 text-destructive"
          }`}>
              <span className={`h-1.5 w-1.5 rounded-full ${
              kernelStatus === "idle" ? "bg-success" :
              kernelStatus === "busy" ? "bg-warning animate-pulse" :
              "bg-muted-foreground"
            }`} />
              {kernelStatus}
            </span>

            {!kernelId ? (
            <KernelConnectButton
              status={kernelStatus}
              language={lang}
              onConnect={(name, pythonPath) => { void kernelOwner.connect({ kind: "local", name, nativeSessionId }, pythonPath); }}
              onRemoteConnect={(url, name, pythonPath) => { void kernelOwner.connect({ kind: "remote", name, url }, pythonPath); }}
            />
          ) : (
              <>
                <Button variant="outline" type="button" className="text-[10px]" onClick={() => void runAllCells()} disabled={running.size > 0 || kernelStatus === "starting" || kernelStatus === "stopping"}>
                  <Play size={10} /> Run All
                </Button>
                <Button variant="outline" type="button" className="text-[10px]" disabled={kernelStatus === "starting" || kernelStatus === "stopping"} onClick={() => void kernelOwner.control("interrupt")} title="Interrupt" aria-label="Interrupt kernel">
                  <Pause size={10} />
                </Button>
                <Button variant="outline" type="button" className="text-[10px]" disabled={kernelStatus === "starting" || kernelStatus === "stopping"} onClick={() => void kernelOwner.control("restart")} title="Restart kernel" aria-label="Restart kernel">
                  <RefreshCw size={10} />
                </Button>
                <Button variant="outline" type="button" className="text-[10px]" disabled={kernelStatus === "stopping"} onClick={() => void kernelOwner.control("disconnect")} title={kernelStatus === "starting" ? "Cancel kernel connection" : "Disconnect"} aria-label={kernelStatus === "starting" ? "Cancel kernel connection" : "Disconnect kernel"}>
                  <Power size={10} />
                </Button>
              </>
            )}
          </div>
        </div>

        {kernelState.error && (
          <p role="alert" className="m-4 break-words rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
            {kernelState.error}
          </p>
        )}
        <div className="mx-auto max-w-5xl space-y-1 p-4">
          {cells.map((cell, i) => (
          <CellView
            key={i}
            cell={cell}
            index={i}
            language={lang}
            kernelConnected={!!kernelId}
            canRun={kernelStatus !== "starting" && kernelStatus !== "stopping"}
            isRunning={running.has(i)}
            executionCount={counts.get(i)}
            liveOutputs={cellOutputs.get(i)}
            onRun={(code) => { void kernelOwner.run(i, code); }}
          />
        ))}
        </div>
      </div>
    </NotebookErrorBoundary>
  );
}

function CellView({ cell, index, language, kernelConnected, canRun, isRunning, executionCount, liveOutputs, onRun }: {
  cell: IpynbCell;
  index: number;
  language: string;
  kernelConnected: boolean;
  canRun: boolean;
  isRunning: boolean;
  executionCount?: number;
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
      <div className="group rounded-lg border border-border/60 bg-card transition hover:border-border">
        <div className="flex items-center gap-2 px-3 py-1.5 text-[10px] text-muted-foreground">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => setCollapsed(!collapsed)}
            className="hover:text-foreground"
            aria-label={collapsed ? "Expand cell" : "Collapse cell"}
          >
            {collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
          </Button>
          <FileText size={11} />
          <span>Markdown</span>
          <span className="ml-auto">[{index + 1}]</span>
        </div>
        {!collapsed && (
          <div className="border-t border-border/40 px-4 py-3">
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
      <div role="group" aria-label={`Notebook cell ${index + 1}`} className="group rounded-lg border border-border/60 bg-card transition hover:border-border">
        {/* Cell header */}
        <div className="flex items-center gap-2 px-3 py-1.5 text-[10px] text-muted-foreground">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => setCollapsed(!collapsed)}
            className="hover:text-foreground"
            aria-label={collapsed ? "Expand cell" : "Collapse cell"}
          >
            {collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
          </Button>
          <Code2 size={11} className="text-primary" />
          <span className="font-mono">{language}</span>
          {/* Cell header */}
          <span className="flex items-center gap-1 rounded bg-background px-1.5 py-[1px] font-mono text-[10px]">
            <Play size={8} className="text-success" />
            {executionCount ?? cell.execution_count ?? " "}
          </span>
          <span className="ml-auto flex items-center gap-2 text-[9px]">
            {kernelConnected && (
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="flex items-center gap-1 rounded bg-success/20 px-1.5 py-0.5 text-success hover:bg-success/30 disabled:opacity-50"
                onClick={() => onRun(source)}
                disabled={isRunning || !canRun}
              >
                {isRunning ? (
                  <Loader2 size={9} className="animate-spin" />
                ) : (
                  <Play size={9} />
                )}
                Run
              </Button>
            )}
            {lineCount} lines
          </span>
        </div>

        {!collapsed && (
          <>
            {/* Code block */}
            <div className="border-t border-border/40 overflow-auto" style={{ maxHeight: 400 }}>
              <pre className="p-3 font-mono text-[12px] leading-5 text-foreground whitespace-pre-wrap">
                <code>{source}</code>
              </pre>
            </div>

            {/* Code block */}
            {(() => {
              const outputs = liveOutputs ?? cell.outputs;
              const hasOutputs = outputs && outputs.length > 0;
              if (!hasOutputs) return null;
              return (
                <div role="region" aria-label={`Cell ${index + 1} output`} className="border-t border-border/40 bg-background p-3">
                  <div className="mb-1 flex items-center gap-1 text-[9px] uppercase tracking-wider text-muted-foreground">
                    <Terminal size={9} /> Output
                    {liveOutputs !== undefined && (
                      <span className="text-success">(live)</span>
                    )}
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
      <div className="rounded-lg border border-border/40 bg-background p-3">
        <div className="mb-1 text-[10px] text-muted-foreground">
          Raw [{index + 1}]
        </div>
        <pre className="font-mono text-[12px] text-foreground">{source}</pre>
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
      <div className="rounded border border-destructive/30 bg-destructive/5 p-2">
        <div className="mb-1 font-mono text-[11px] font-bold text-destructive">
          {output.ename}: {output.evalue}
        </div>
        <pre className="max-h-[200px] overflow-auto font-mono text-[11px] text-destructive/80">
          {clean}
        </pre>
      </div>
    );
  }

  // Stream output (stdout/stderr).
  if (output.output_type === "stream") {
    const text = Array.isArray(output.text)
      ? output.text.join("")
      : (output.text ?? "");
    const isErr = output.name === "stderr";
    return (
      <pre className={`max-h-[300px] overflow-auto font-mono text-[12px] ${isErr ? "text-warning" : "text-foreground"}`}>
        {text}
      </pre>
    );
  }

  // Display data / execute_result.
  const data = output.data;
  if (!data) {
    const text = Array.isArray(output.text) ? output.text.join("") : output.text;
    if (text)
      return (
        <pre className="font-mono text-[12px] text-foreground">{text}</pre>
      );
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
      {png && (
        <img src={`data:image/png;base64,${png}`} alt="output" className="max-w-full rounded" />
      )}
      {jpeg && !png && (
        <img src={`data:image/jpeg;base64,${jpeg}`} alt="output" className="max-w-full rounded" />
      )}
      {svg && !png && !jpeg && <NotebookRichOutput kind="SVG" content={svg} />}
      {html && !png && !jpeg && !svg && (
        <NotebookRichOutput kind="HTML" content={html} />
      )}
      {latex && !html && !png && !svg && (
        <div className="font-mono text-[12px] text-foreground">{latex}</div>
      )}
      {plain && !png && !jpeg && !svg && !html && (
        <pre className="max-h-[300px] overflow-auto font-mono text-[12px] text-foreground">
          {plain}
        </pre>
      )}
    </div>
  );
}

function NotebookRichOutput({ kind, content }: { kind: "HTML" | "SVG"; content: string }) {
  const theme = useEffectiveTheme();
  // Saved/live markup is data, not code for the privileged renderer. Keep
  // script and same-origin exemptions absent so it cannot reach window.crab.
  const source = `<!doctype html><html><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:; base-uri 'none'; form-action 'none'"><meta name="color-scheme" content="${theme}"><style>:root{color-scheme:${theme}}body{margin:0;color:CanvasText;background:Canvas;font:13px/1.5 system-ui;overflow-wrap:anywhere}img,svg{max-width:100%}svg{fill:currentColor}table{border-collapse:collapse}th,td{padding:.25rem .5rem;text-align:start}</style></head><body>${content}</body></html>`;
  return (
    <iframe title={`Notebook ${kind} output`} sandbox="" referrerPolicy="no-referrer" srcDoc={source} className="h-48 min-h-24 w-full resize-y rounded border-0 bg-background" />
  );
}

function joinData(d: string | string[] | undefined): string | null {
  if (!d) return null;
  return Array.isArray(d) ? d.join("") : d;
}

function KernelConnectButton({ status, language, onConnect, onRemoteConnect }: {
  status: string;
  language: string;
  onConnect: (name: string, pythonPath: string) => void;
  onRemoteConnect: (url: string, name: string, pythonPath: string) => void;
}) {
  const [showPanel, setShowPanel] = useState(false);
  const [checking, setChecking] = useState(false);
  const [runtime, setRuntime] = useState<JupyterRuntime | null>(null);
  const [pythonPath, setPythonPath] = useState("");
  const [specs, setSpecs] = useState<KernelSpec[]>([]);
  const [selected, setSelected] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [installing, setInstalling] = useState(false);
  const [remoteUrl, setRemoteUrl] = useState("");
  const [remoteName, setRemoteName] = useState("");

  const check = async (value?: string) => {
    setChecking(true); setError(null); setSpecs([]);
    try {
      const result = await window.crab.jupyterCheck(value);
      setRuntime(result);
      if (result.pythonPath) setPythonPath(result.pythonPath);
      if (result.error) setError(result.error);
      if (result.available && result.pythonPath) {
        const available = await window.crab.jupyterListKernels(result.pythonPath);
        setSpecs(available);
        setSelected(available.find(spec => spec.language === language)?.name ?? available[0]?.name ?? "");
      }
    } catch (failure) { setError(String(failure)); }
    finally { setChecking(false); }
  };
  const install = async () => {
    if (!runtime?.pythonPath) return;
    setInstalling(true); setError(null);
    try { await window.crab.jupyterInstall(runtime.pythonPath); await check(runtime.pythonPath); }
    catch (failure) { setError(String(failure)); }
    finally { setInstalling(false); }
  };

  if (!showPanel)
    return (
      <Button variant="default" type="button" className="text-[10px]" onClick={() => { setShowPanel(true); void check(); }} disabled={status === "starting"}>
        <Zap size={10} /> Connect Kernel
      </Button>
    );
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) setShowPanel(false);
      }}
    >
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className="w-80 space-y-3 p-4"
      >
        <DialogTitle className="sr-only">Connect to kernel</DialogTitle>
        <div className="flex items-center justify-between">
          <span className="text-[12px] font-semibold text-foreground">
            Connect to kernel
          </span>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => setShowPanel(false)}
            className="text-muted-foreground hover:text-foreground text-[11px]"
            aria-label="Close panel"
          >
            ✕
          </Button>
        </div>
        <label className="block space-y-1 text-[11px]">
          Python executable
          <Input className="input w-full" value={pythonPath} placeholder="Python executable or virtual environment path" onChange={event => { setPythonPath(event.target.value); setRuntime(null); }} />
        </label>
        <Button variant="outline" type="button" className="text-[11px]" disabled={checking || installing} onClick={() => { void check(pythonPath || undefined); }}>
          {checking ? "Checking runtime…" : "Check runtime"}
        </Button>
        {error && (
          <p role="alert" className="break-words text-[11px] text-destructive">
            {error}
          </p>
        )}
        {runtime?.pythonPath && !runtime.available && (
          <div className="space-y-2 text-[11px]">
            <p>Jupyter Client is not installed in this Python environment.</p>
            <Button variant="outline" type="button" className="w-full" disabled={installing} onClick={() => { void install(); }}>
              {installing ? "Installing…" : "Install Jupyter Client + ipykernel"}
            </Button>
          </div>
        )}
        {runtime?.available && specs.length > 0 && (
          <div className="space-y-2">
            <label className="block text-[11px]">
              Kernel
              <Select
                value={":" + String(selected)}
                onValueChange={(selectedValue) => {
                  const value = selectedValue.slice(1);
                  return setSelected(value);
                }}
              >
                <SelectTrigger className="input w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectGroup>
                    {specs.map((spec) => (
                      <SelectItem
                        key={spec.name}
                        value={":" + String(spec.name)}
                      >
                        {spec.displayName}
                      </SelectItem>
                    ))}
                  </SelectGroup>
                </SelectContent>
              </Select>
            </label>
            <Button variant="default" type="button" className="w-full text-[11px]" onClick={() => { if (!runtime.pythonPath) return; setShowPanel(false); onConnect(selected, runtime.pythonPath); }}>
              <Play size={10} /> Start Local Kernel
            </Button>
          </div>
        )}
        {runtime?.available && !checking && !specs.length && !error && (
          <p className="text-[11px] text-warning">
            No kernels are installed in this environment.
          </p>
        )}

        <div className="space-y-2 border-t border-border pt-3 text-[11px]">
          <label className="block space-y-1">
            Jupyter server URL
            <Input
            className="input w-full"
            type="password"
            autoComplete="off"
            placeholder="https://server.example/?token=…"
            value={remoteUrl}
            onChange={event => setRemoteUrl(event.target.value)}
          />
          </label>
          <label className="block space-y-1">
            Remote kernel name
            <Input className="input w-full" placeholder="Server default" value={remoteName} onChange={event => setRemoteName(event.target.value)} />
          </label>
          <p className="text-muted-foreground">
            Code runs on the server. Place any required files there before
            running cells.
          </p>
          <Button variant="outline" type="button" disabled={!remoteUrl.trim() || !runtime?.available || checking || installing} onClick={() => {
          if (!runtime?.pythonPath) return;
          const url = remoteUrl.trim();
          setRemoteUrl("");
          setShowPanel(false);
          onRemoteConnect(url, remoteName.trim(), runtime.pythonPath);
        }}>
            Connect to server
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

// Error boundary to prevent notebook rendering crashes from killing the app.
class NotebookErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null };
  static getDerivedStateFromError(error: Error) { return { error }; }
  render() {
    if (this.state.error) {
      return (
        <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-4 text-xs text-destructive">
          <div className="mb-2 font-semibold">Notebook rendering error</div>
          <pre className="whitespace-pre-wrap">{this.state.error.message}</pre>
          <Button variant="outline"
            type="button"
            className="mt-3 text-xs"
            onClick={() => this.setState({ error: null })}
          >
            Retry
          </Button>
        </div>
      );
    }
    return this.props.children;
  }
}
