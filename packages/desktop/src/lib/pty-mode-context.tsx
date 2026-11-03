/**
 * PtyModeContext — renderer-side view of the resolved PTY mode.
 *
 * Fetches `crab:pty-mode` once on app boot and exposes `daemonReady`
 * (plus the full mode payload) to any component that needs to decide
 * whether daemon-backed affordances should be enabled or disabled.
 *
 * Usage:
 *   // Wrap the app root:
 *   <PtyModeProvider>…</PtyModeProvider>
 *
 *   // Consume anywhere:
 *   const { daemonReady } = usePtyMode();
 */

import { createContext, useContext, useEffect, useState, type ReactNode } from "react";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface PtyModeState {
  /** True when the daemon is the resolved runtime mode. */
  daemonReady: boolean;
  /** True when the circuit breaker is open (daemon temporarily unavailable). */
  breakerOpen: boolean;
  /** The resolved mode string from mode-selection.ts. */
  resolvedMode: "daemon" | "in-process";
  /** The configured mode from the CRAB_PTY_MODE env var. */
  configuredMode: "auto" | "daemon" | "in-process";
  /** Non-null when auto mode fell back to in-process. */
  fallback: { reason: string; error?: string; timestamp: number } | null;
  /** True while the initial IPC call is in flight. */
  loading: boolean;
}

const defaultState: PtyModeState = {
  daemonReady: false,
  breakerOpen: false,
  resolvedMode: "in-process",
  configuredMode: "auto",
  fallback: null,
  loading: true,
};

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

const PtyModeContext = createContext<PtyModeState>(defaultState);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

export function PtyModeProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<PtyModeState>(defaultState);

  useEffect(() => {
    let cancelled = false;

    // One-time fetch to get the current mode on mount.
    void (async () => {
      try {
        const result = await (window.crab as any).getPtyMode() as {
          resolvedMode: "daemon" | "in-process";
          configuredMode: "auto" | "daemon" | "in-process";
          fallback: { reason: string; error?: string; timestamp: number } | null;
        };
        if (cancelled) return;
        setState({
          daemonReady: result.resolvedMode === "daemon",
          breakerOpen: false,
          resolvedMode: result.resolvedMode,
          configuredMode: result.configuredMode,
          fallback: result.fallback,
          loading: false,
        });
      } catch {
        // IPC unavailable (e.g. running in a browser dev environment) —
        // treat as in-process mode so the UI degrades gracefully.
        if (cancelled) return;
        setState({
          daemonReady: false,
          breakerOpen: false,
          resolvedMode: "in-process",
          configuredMode: "auto",
          fallback: null,
          loading: false,
        });
      }
    })();

    // Subscribe to live push events so the context stays current when the
    // daemon connects/disconnects or the circuit breaker changes state.
    let offPtyMode: (() => void) | undefined;
    const crab = window.crab as any;
    if (typeof crab?.onPtyMode === "function") {
      offPtyMode = crab.onPtyMode((mode: { daemonReady: boolean; breakerOpen: boolean }) => {
        if (cancelled) return;
        setState((prev) => ({
          ...prev,
          daemonReady: mode.daemonReady,
          breakerOpen: mode.breakerOpen,
          // Keep the rest of the fields from the last full fetch; the push
          // event only carries the fields that change at runtime.
          loading: false,
        }));
      });
    }

    return () => {
      cancelled = true;
      offPtyMode?.();
    };
  }, []);

  return <PtyModeContext.Provider value={state}>{children}</PtyModeContext.Provider>;
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

/**
 * Returns the current PTY mode state.
 *
 * `daemonReady` is the primary flag components should check before
 * enabling daemon-specific affordances (e.g. "New daemon terminal").
 */
export function usePtyMode(): PtyModeState {
  return useContext(PtyModeContext);
}
