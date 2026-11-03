import React, { Component, type ReactNode } from "react";
import ReactDOM from "react-dom/client";
import "./lib/monaco-env";
import { preloadDiffThemes } from "./lib/diffTheme";
import App from "./App";
import "@xyflow/react/dist/style.css";
import "./index.css";

// Kick off theme preload so the first diff render gets its CSS
// from the cache instead of racing a fetch.
preloadDiffThemes();

// Detect the Vite "stale chunk" error: a renderer launched against an
// older bundle tries to dynamically import a chunk whose filename hash
// has since been rebuilt. The fix is to reload the window so the fresh
// HTML + bundle references take over. Streamdown (used by ChatTab,
// MarkdownView, NotebookView) lazy-loads its syntax highlighter on the
// first fenced code block — that's the usual trigger point.
const CHUNK_LOAD_PATTERNS = [
  "Failed to fetch dynamically imported module",
  "error loading dynamically imported module",
  "Importing a module script failed",
];

function isStaleChunkError(err: unknown): boolean {
  if (!err) return false;
  const message = err instanceof Error ? err.message : String(err);
  return CHUNK_LOAD_PATTERNS.some((p) => message.includes(p));
}

// Guard against reload loops: if a reload itself fails to recover
// (e.g. the chunk genuinely no longer exists), we fall back to the
// manual recovery UI after one attempt.
const RELOAD_FLAG = "crab.chunk-reload-attempt";

function reloadOnce(): boolean {
  try {
    const already = sessionStorage.getItem(RELOAD_FLAG);
    if (already) return false;
    sessionStorage.setItem(RELOAD_FLAG, String(Date.now()));
  } catch {
    // sessionStorage disabled — still attempt reload rather than crash.
  }
  window.location.reload();
  return true;
}

// Clear the reload flag on a clean boot so the *next* stale-chunk
// incident also gets its one automatic recovery attempt.
try {
  const flag = sessionStorage.getItem(RELOAD_FLAG);
  if (flag) {
    const ts = Number(flag);
    // If the flag is older than 30s, assume the last reload succeeded
    // and clear it so future crashes can auto-recover again.
    if (!Number.isFinite(ts) || Date.now() - ts > 30_000) {
      sessionStorage.removeItem(RELOAD_FLAG);
    }
  }
} catch {
  // ignore
}

// Catch rejections from dynamic imports that happen outside a React
// render (e.g. event handlers, effects). Without this, they surface
// as a blank screen log entry rather than the error boundary.
window.addEventListener("unhandledrejection", (event) => {
  if (isStaleChunkError(event.reason)) {
    event.preventDefault();
    reloadOnce();
  }
});

window.addEventListener("error", (event) => {
  if (isStaleChunkError(event.error) || isStaleChunkError(event.message)) {
    event.preventDefault();
    reloadOnce();
  }
});

// Top-level error boundary so a renderer crash shows a recovery
// UI instead of a blank window.
class AppErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null }> {
  state = { error: null as Error | null };
  static getDerivedStateFromError(error: Error) { return { error }; }
  componentDidCatch(error: Error) {
    // Stale chunk after a rebuild — reload once automatically so the
    // user doesn't have to click through the recovery UI.
    if (isStaleChunkError(error)) {
      reloadOnce();
    }
  }
  render() {
    if (this.state.error) {
      const stale = isStaleChunkError(this.state.error);
      return (
        <div style={{ padding: 40, fontFamily: "system-ui", color: "#e7eaf1", background: "#0f1115", height: "100vh" }}>
          <h1 style={{ fontSize: 20, marginBottom: 12 }}>Crab Desktop crashed</h1>
          {stale && (
            <p style={{ fontSize: 13, color: "#fbbf24", marginBottom: 16 }}>
              This looks like a stale build. Reloading usually fixes it.
            </p>
          )}
          <pre style={{ fontSize: 12, whiteSpace: "pre-wrap", color: "#f87171", marginBottom: 20 }}>
            {this.state.error.message}
            {"\n\n"}
            {this.state.error.stack}
          </pre>
          <button
            onClick={() => {
              try { sessionStorage.removeItem(RELOAD_FLAG); } catch {}
              window.location.reload();
            }}
            style={{ padding: "8px 16px", background: "#4f8cff", color: "white", border: "none", borderRadius: 6, cursor: "pointer", fontSize: 13 }}
          >
            Reload
          </button>
          <button
            onClick={() => {
              // Clear persisted state that might be causing the crash.
              try { localStorage.clear(); } catch {}
              try { sessionStorage.clear(); } catch {}
              window.location.reload();
            }}
            style={{ padding: "8px 16px", background: "#242832", color: "#e7eaf1", border: "1px solid #363a46", borderRadius: 6, cursor: "pointer", fontSize: 13, marginLeft: 8 }}
          >
            Clear state &amp; reload
          </button>
          <button
            onClick={() => this.setState({ error: null })}
            style={{ padding: "8px 16px", background: "#242832", color: "#e7eaf1", border: "1px solid #363a46", borderRadius: 6, cursor: "pointer", fontSize: 13, marginLeft: 8 }}
          >
            Try again
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <AppErrorBoundary>
      <App />
    </AppErrorBoundary>
  </React.StrictMode>,
);
