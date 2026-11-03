/**
 * Quick Terminal overlay component.
 *
 * A dropdown terminal that slides in from the top of the application window,
 * toggled via a global hotkey (default: Ctrl+`). Operates independently of
 * the main terminal view and persists its session across app restarts.
 */

import { useEffect,useRef,useState } from "react";
import clsx from "clsx";

// --- Types ---

export interface QuickTerminalConfig {
  /** Hotkey to toggle the Quick Terminal (default: "Ctrl+`"). */
  hotkey: string;
  /** Height as percentage of window height (20–80, default: 40). */
  heightPercent: number;
  /** Animation duration in milliseconds (default: 150). */
  animationDurationMs: number;
}

export interface QuickTerminalSession {
  /** Whether the Quick Terminal is currently visible. */
  visible: boolean;
  /** The PTY session ID (null if not spawned). */
  sessionId: string | null;
  /** Working directory inherited from last-active repo. */
  cwd: string;
  /** Environment context from last-active repo. */
  env: Record<string, string>;
}

// --- Constants ---

export const DEFAULT_QUICK_TERMINAL_CONFIG: QuickTerminalConfig = {
  hotkey: "Ctrl+`",
  heightPercent: 40,
  animationDurationMs: 150,
};

const LS_QUICK_TERMINAL_KEY = "crab.quickTerminal";

// --- Height Calculation ---

/**
 * Calculate the Quick Terminal overlay height in pixels.
 *
 * For any configured height percentage P (20–80) and window height H,
 * the overlay height equals floor(H × P / 100) pixels.
 */
export function calculateOverlayHeight(
  heightPercent: number,
  windowHeight: number,
): number {
  // Clamp percentage to valid range
  const clamped = Math.max(20, Math.min(80, heightPercent));
  return Math.floor(windowHeight * clamped / 100);
}

// --- Persistence ---

export function loadQuickTerminalState(): { active: boolean; cwd?: string } {
  try {
    const raw = localStorage.getItem(LS_QUICK_TERMINAL_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      return {
        active: parsed.active ?? false,
        cwd: parsed.cwd,
      };
    }
  } catch {
    // Fall through
  }
  return { active: false };
}

export function saveQuickTerminalState(active: boolean, cwd?: string): void {
  localStorage.setItem(LS_QUICK_TERMINAL_KEY, JSON.stringify({ active, cwd }));
}

// --- Hotkey Matching ---

/**
 * Check if a keyboard event matches the configured hotkey string.
 */
export function matchesHotkey(event: KeyboardEvent, hotkey: string): boolean {
  const parts = hotkey.toLowerCase().split("+").map((p) => p.trim());
  const key = parts[parts.length - 1];
  const modifiers = parts.slice(0, -1);

  // Check key match
  if (event.key.toLowerCase() !== key && event.code.toLowerCase() !== key.replace("`", "backquote")) {
    // Special handling for backtick
    if (key === "`" && event.key !== "`" && event.code !== "Backquote") {
      return false;
    }
    if (key !== "`" && event.key.toLowerCase() !== key) {
      return false;
    }
  }

  // Check modifiers
  const needsCtrl = modifiers.includes("ctrl") || modifiers.includes("cmdorctrl");
  const needsShift = modifiers.includes("shift");
  const needsAlt = modifiers.includes("alt");
  const needsMeta = modifiers.includes("cmd") || modifiers.includes("meta") || modifiers.includes("cmdorctrl");

  if (needsCtrl && !event.ctrlKey && !event.metaKey) return false;
  if (needsShift && !event.shiftKey) return false;
  if (needsAlt && !event.altKey) return false;
  if (needsMeta && !event.metaKey && !event.ctrlKey) return false;

  return true;
}

// --- Component ---

interface QuickTerminalProps {
  /** Last-active repo's working directory. */
  repoCwd: string;
  /** Last-active repo's environment context. */
  repoEnv: Record<string, string>;
  /** Configuration for the Quick Terminal. */
  config?: QuickTerminalConfig;
  /** Callback to spawn a PTY session. */
  onSpawn?: (cwd: string, env: Record<string, string>) => Promise<string>;
  /** Callback when the terminal is toggled. */
  onToggle?: (visible: boolean) => void;
}

export function QuickTerminal({
  repoCwd,
  repoEnv,
  config = DEFAULT_QUICK_TERMINAL_CONFIG,
  onSpawn,
  onToggle,
}: QuickTerminalProps) {
  const [visible, setVisible] = useState(false);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const animatingRef = useRef(false);

  // Calculate height
  const overlayHeight = calculateOverlayHeight(
    config.heightPercent,
    typeof window !== "undefined" ? window.innerHeight : 800,
  );

  // Register global hotkey listener
  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      if (matchesHotkey(e, config.hotkey)) {
        e.preventDefault();
        e.stopPropagation();
        toggle();
      }
    }

    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [config.hotkey]);

  // Restore session on mount if one was active at shutdown
  useEffect(() => {
    const saved = loadQuickTerminalState();
    if (saved.active) {
      setVisible(true);
      void spawnSession(saved.cwd || repoCwd);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Click outside to dismiss
  useEffect(() => {
    if (!visible) return;

    function handleClickOutside(e: MouseEvent) {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        toggle();
      }
    }

    // Delay to avoid catching the toggle click itself
    const timer = setTimeout(() => {
      document.addEventListener("mousedown", handleClickOutside);
    }, 100);

    return () => {
      clearTimeout(timer);
      document.removeEventListener("mousedown", handleClickOutside);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visible]);

  async function spawnSession(cwd: string) {
    if (!onSpawn || sessionId) return;
    try {
      const id = await onSpawn(cwd, repoEnv);
      setSessionId(id);
    } catch (err) {
      console.error("[QuickTerminal] Failed to spawn session:", err);
    }
  }

  function toggle() {
    if (animatingRef.current) return;
    animatingRef.current = true;

    const newVisible = !visible;
    setVisible(newVisible);
    saveQuickTerminalState(newVisible, repoCwd);
    onToggle?.(newVisible);

    if (newVisible && !sessionId) {
      void spawnSession(repoCwd);
    }

    setTimeout(() => {
      animatingRef.current = false;
    }, config.animationDurationMs);
  }

  if (!visible) return null;

  return (
    <div
      ref={containerRef}
      className={clsx(
        "fixed top-0 left-0 right-0 z-50",
        "bg-[var(--color-bg-primary)] border-b border-[var(--color-border)]",
        "shadow-lg transition-transform",
      )}
      style={{
        height: `${overlayHeight}px`,
        transitionDuration: `${config.animationDurationMs}ms`,
        transform: visible ? "translateY(0)" : "translateY(-100%)",
      }}
      data-testid="quick-terminal-overlay"
    >
      {/* Header bar */}
      <div className="flex items-center justify-between px-3 py-1 border-b border-[var(--color-border)] bg-[var(--color-bg-secondary)]">
        <span className="text-xs text-[var(--color-text-secondary)] font-medium">
          Quick Terminal
        </span>
        <span className="text-xs text-[var(--color-text-tertiary)]">
          {config.hotkey} to dismiss
        </span>
      </div>

      {/* Terminal container */}
      <div className="flex-1 h-[calc(100%-28px)]" data-testid="quick-terminal-content">
        {sessionId ? (
          <div className="w-full h-full" data-terminal-id={sessionId} />
        ) : (
          <div className="flex items-center justify-center h-full text-[var(--color-text-tertiary)] text-sm">
            Starting terminal session...
          </div>
        )}
      </div>
    </div>
  );
}
