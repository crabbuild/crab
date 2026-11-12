/**
 * Provider-aware agent icons using official brand SVG paths.
 *
 * Sources (all permissively licensed):
 * - Claude: Simple Icons (CC0 1.0) — https://simpleicons.org/?q=claude
 * - OpenAI: Bootstrap Icons (MIT) — https://icons.getbootstrap.com/icons/openai/
 * - Gemini: Simple Icons (CC0 1.0) — https://simpleicons.org/?q=googlegemini
 * - OpenCode, Qwen, Aider: stylized marks (no official SVG available)
 */

import clsx from "clsx";

// ── Provider resolution ─────────────────────────────────────────────

export type AgentProvider =
  | "claude"
  | "codex"
  | "gemini"
  | "opencode"
  | "qwen"
  | "aider"
  | "kiro"
  | "q"
  | "custom";

/**
 * Normalizes an agent name/type (e.g. "claude-code", "Claude Code", "gpt-5-codex")
 * to a known provider. Lowercases and checks common aliases so the UI looks right
 * whether the caller passes the registry type, the display name, or a model id.
 */
export function resolveAgentProvider(agentName: string | undefined | null): AgentProvider {
  const n = (agentName ?? "").toLowerCase();
  if (!n) return "custom";
  if (n.includes("claude") || n === "anthropic") return "claude";
  if (n.includes("codex") || n.includes("openai") || n.startsWith("gpt-")) return "codex";
  if (n.includes("gemini") || n.includes("google")) return "gemini";
  if (n.includes("opencode")) return "opencode";
  if (n.includes("qwen") || n.includes("dashscope") || n.includes("alibaba")) return "qwen";
  if (n.includes("aider")) return "aider";
  if (n.includes("kiro")) return "kiro";
  if (n === "q" || n.includes("amazon q") || n.includes("q chat") || n.includes("q developer")) return "q";
  return "custom";
}

// ── Accent colors ───────────────────────────────────────────────────

/** Foreground color class for the provider's mark. */
export function agentAccentColor(agentName: string | undefined | null): string {
  switch (resolveAgentProvider(agentName)) {
    case "claude":
      return "text-[#da7756]";
    case "codex":
      return "text-[#10a37f]";
    case "gemini":
      return "text-[#8e75b2]";
    case "opencode":
      return "text-[#c084fc]";
    case "qwen":
      return "text-[#7c5cff]";
    case "aider":
      return "text-sky-400";
    case "kiro":
      return "text-[#ff9900]";
    case "q":
      return "text-[#527fff]";
    case "custom":
    default:
      return "text-primary";
  }
}

/** Background tint class for the avatar bubble. */
export function agentAccentBg(agentName: string | undefined | null): string {
  switch (resolveAgentProvider(agentName)) {
    case "claude":
      return "bg-[#da7756]/15";
    case "codex":
      return "bg-[#10a37f]/15";
    case "gemini":
      return "bg-[#8e75b2]/15";
    case "opencode":
      return "bg-[#c084fc]/15";
    case "qwen":
      return "bg-[#7c5cff]/15";
    case "aider":
      return "bg-sky-400/15";
    case "kiro":
      return "bg-[#ff9900]/15";
    case "q":
      return "bg-[#527fff]/15";
    case "custom":
    default:
      return "bg-primary/15";
  }
}

/** Human-friendly display label for a provider. */
export function agentProviderLabel(agentName: string | undefined | null): string {
  switch (resolveAgentProvider(agentName)) {
    case "claude":
      return "Claude";
    case "codex":
      return "Codex";
    case "gemini":
      return "Gemini";
    case "opencode":
      return "OpenCode";
    case "qwen":
      return "Qwen";
    case "aider":
      return "Aider";
    case "kiro":
      return "Kiro";
    case "q":
      return "Amazon Q";
    case "custom":
    default:
      return "Custom";
  }
}

// ── Icon component ──────────────────────────────────────────────────

interface Props {
  /** Registry type ("claude-code") or display name ("Claude Code"). */
  agentName: string | undefined | null;
  size?: number;
  className?: string;
}

/**
 * Renders the brand mark for the given agent. Always wraps the SVG with a
 * `shrink-0` class so it plays well inside flex rows.
 */
export function AgentIcon({ agentName, size = 12, className }: Props) {
  const provider = resolveAgentProvider(agentName);
  const cls = clsx("shrink-0", className);

  switch (provider) {
    case "claude":
      return <ClaudeMark size={size} className={cls} />;
    case "codex":
      return <OpenAIMark size={size} className={cls} />;
    case "gemini":
      return <GeminiMark size={size} className={cls} />;
    case "opencode":
      return <OpenCodeMark size={size} className={cls} />;
    case "qwen":
      return <QwenMark size={size} className={cls} />;
    case "aider":
      return <AiderMark size={size} className={cls} />;
    case "kiro":
      return <KiroMark size={size} className={cls} />;
    case "q":
      return <AmazonQMark size={size} className={cls} />;
    case "custom":
    default:
      return <CustomMark size={size} className={cls} />;
  }
}

// ── Individual marks ───────────────────────────────────────────────

type MarkProps = { size: number; className?: string };

/**
 * Anthropic Claude — official logo from Bootstrap Icons (MIT).
 * Source: https://icons.getbootstrap.com/icons/claude/
 */
function ClaudeMark({ size, className }: MarkProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" fill="currentColor" className={className} aria-hidden>
      <path d="m3.127 10.604 3.135-1.76.053-.153-.053-.085H6.11l-.525-.032-1.791-.048-1.554-.065-1.505-.08-.38-.081L0 7.832l.036-.234.32-.214.455.04 1.009.069 1.513.105 1.097.064 1.626.17h.259l.036-.105-.089-.065-.068-.064-1.566-1.062-1.695-1.121-.887-.646-.48-.327-.243-.306-.104-.67.435-.48.585.04.15.04.593.456 1.267.981 1.654 1.218.242.202.097-.068.012-.049-.109-.181-.9-1.626-.96-1.655-.428-.686-.113-.411a2 2 0 0 1-.068-.484l.496-.674L4.446 0l.662.089.279.242.411.94.666 1.48 1.033 2.014.302.597.162.553.06.17h.105v-.097l.085-1.134.157-1.392.154-1.792.052-.504.25-.605.497-.327.387.186.319.456-.045.294-.19 1.23-.37 1.93-.243 1.29h.142l.161-.16.654-.868 1.097-1.372.484-.545.565-.601.363-.287h.686l.505.751-.226.775-.707.895-.585.759-.839 1.13-.524.904.048.072.125-.012 1.897-.403 1.024-.186 1.223-.21.553.258.06.263-.218.536-1.307.323-1.533.307-2.284.54-.028.02.032.04 1.029.098.44.024h1.077l2.005.15.525.346.315.424-.053.323-.807.411-3.631-.863-.872-.218h-.12v.073l.726.71 1.331 1.202 1.667 1.55.084.383-.214.302-.226-.032-1.464-1.101-.565-.497-1.28-1.077h-.084v.113l.295.432 1.557 2.34.08.718-.112.234-.404.141-.444-.08-.911-1.28-.94-1.44-.759-1.291-.093.053-.448 4.821-.21.246-.484.186-.403-.307-.214-.496.214-.98.258-1.28.21-1.016.19-1.263.112-.42-.008-.028-.092.012-.953 1.307-1.448 1.957-1.146 1.227-.274.109-.477-.247.045-.44.266-.39 1.586-2.018.956-1.25.617-.723-.004-.105h-.036l-4.212 2.736-.75.096-.324-.302.04-.496.154-.162 1.267-.871z" />
    </svg>
  );
}

/**
 * OpenAI (Codex) — official logo from Bootstrap Icons (MIT).
 * Source: https://icons.getbootstrap.com/icons/openai/
 */
function OpenAIMark({ size, className }: MarkProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" fill="currentColor" className={className} aria-hidden>
      <path d="M14.949 6.547a3.94 3.94 0 0 0-.348-3.273 4.11 4.11 0 0 0-4.4-1.934A4.1 4.1 0 0 0 8.423.2 4.15 4.15 0 0 0 6.305.086a4.1 4.1 0 0 0-1.891.948 4.04 4.04 0 0 0-1.158 1.753 4.1 4.1 0 0 0-1.563.679A4 4 0 0 0 .554 4.72a3.99 3.99 0 0 0 .502 4.731 3.94 3.94 0 0 0 .346 3.274 4.11 4.11 0 0 0 4.402 1.933c.382.425.852.764 1.377.995.526.231 1.095.35 1.67.346 1.78.002 3.358-1.132 3.901-2.804a4.1 4.1 0 0 0 1.563-.68 4 4 0 0 0 1.14-1.253 3.99 3.99 0 0 0-.506-4.716m-6.097 8.406a3.05 3.05 0 0 1-1.945-.694l.096-.054 3.23-1.838a.53.53 0 0 0 .265-.455v-4.49l1.366.778q.02.011.025.035v3.722c-.003 1.653-1.361 2.992-3.037 2.996m-6.53-2.75a2.95 2.95 0 0 1-.36-2.01l.095.057L5.29 12.09a.53.53 0 0 0 .527 0l3.949-2.246v1.555a.05.05 0 0 1-.022.041L6.473 13.3c-1.454.826-3.311.335-4.15-1.098m-.85-6.94A3.02 3.02 0 0 1 3.07 3.949v3.785a.51.51 0 0 0 .262.451l3.93 2.237-1.366.779a.05.05 0 0 1-.048 0L2.585 9.342a2.98 2.98 0 0 1-1.113-4.094zm11.216 2.571L8.747 5.576l1.362-.776a.05.05 0 0 1 .048 0l3.265 1.86a3 3 0 0 1 1.173 1.207 2.96 2.96 0 0 1-.27 3.2 3.05 3.05 0 0 1-1.36.997V8.279a.52.52 0 0 0-.276-.445m1.36-2.015-.097-.057-3.226-1.855a.53.53 0 0 0-.53 0L6.249 6.153V4.598a.04.04 0 0 1 .019-.04L9.533 2.7a3.07 3.07 0 0 1 3.257.139c.474.325.843.778 1.066 1.303.223.526.289 1.103.191 1.664zM5.503 8.575 4.139 7.8a.05.05 0 0 1-.026-.037V4.049c0-.57.166-1.127.476-1.607s.752-.864 1.275-1.105a3.08 3.08 0 0 1 3.234.41l-.096.054-3.23 1.838a.53.53 0 0 0-.265.455zm.742-1.577 1.758-1 1.762 1v2l-1.755 1-1.762-1z" />
    </svg>
  );
}

/**
 * Google Gemini — official logo from Simple Icons (CC0 1.0).
 * Source: https://simpleicons.org/?q=googlegemini
 */
function GeminiMark({ size, className }: MarkProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="currentColor" className={className} aria-hidden>
      <path d="M11.04 19.32Q12 21.51 12 24q0-2.49.93-4.68q.96-2.19 2.58-3.81t3.81-2.55Q21.51 12 24 12q-2.49 0-4.68-.93a12.3 12.3 0 0 1-3.81-2.58a12.3 12.3 0 0 1-2.58-3.81Q12 2.49 12 0q0 2.49-.96 4.68q-.93 2.19-2.55 3.81a12.3 12.3 0 0 1-3.81 2.58Q2.49 12 0 12q2.49 0 4.68.96q2.19.93 3.81 2.55t2.55 3.81" />
    </svg>
  );
}

/**
 * OpenCode — no official SVG available. Stylized code brackets in a tile.
 */
function OpenCodeMark({ size, className }: MarkProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" className={className} aria-hidden>
      <rect x="3" y="4" width="18" height="16" rx="3.5" fill="currentColor" fillOpacity="0.18" />
      <path
        d="M9 9l-3 3 3 3"
        stroke="currentColor"
        strokeWidth="1.8"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="M15 9l3 3-3 3"
        stroke="currentColor"
        strokeWidth="1.8"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/**
 * Alibaba Qwen — no official SVG available. Stylized "Q" ring with tail.
 */
function QwenMark({ size, className }: MarkProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" className={className} aria-hidden>
      <circle
        cx="11"
        cy="11"
        r="6.5"
        stroke="currentColor"
        strokeWidth="2"
        fill="currentColor"
        fillOpacity="0.12"
      />
      <path
        d="M14.5 14.5l4.5 4.5"
        stroke="currentColor"
        strokeWidth="2.2"
        strokeLinecap="round"
      />
    </svg>
  );
}

/**
 * Aider — no official SVG available. Chat bubble with terminal prompt.
 */
function AiderMark({ size, className }: MarkProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" className={className} aria-hidden>
      <path
        d="M4 6.5A2.5 2.5 0 0 1 6.5 4h11A2.5 2.5 0 0 1 20 6.5V14a2.5 2.5 0 0 1-2.5 2.5H10l-4 3.2V16.5H6.5A2.5 2.5 0 0 1 4 14V6.5Z"
        fill="currentColor"
        fillOpacity="0.15"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinejoin="round"
      />
      <path
        d="M8 8l2.5 2.3L8 12.5"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path d="M12 12.5h4" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  );
}

/**
 * Kiro — AWS Kiro IDE brand mark. Stylized "K" lightning bolt.
 */
function KiroMark({ size, className }: MarkProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" className={className} aria-hidden>
      <path
        d="M6 4h3l-1 7h4l-5 9 1.5-6H5.5L6 4Z"
        fill="currentColor"
        fillOpacity="0.2"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinejoin="round"
      />
      <path
        d="M13 4h5v3l-4 5 4 5v3h-5"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/**
 * Amazon Q Developer — stylized "Q" mark inspired by the Amazon Q brand.
 */
function AmazonQMark({ size, className }: MarkProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" className={className} aria-hidden>
      <circle
        cx="12"
        cy="11"
        r="7"
        fill="currentColor"
        fillOpacity="0.15"
        stroke="currentColor"
        strokeWidth="1.8"
      />
      <path
        d="M14 14l4 4"
        stroke="currentColor"
        strokeWidth="2.2"
        strokeLinecap="round"
      />
      <path
        d="M9.5 11c0-1.38 1.12-2.5 2.5-2.5s2.5 1.12 2.5 2.5"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
      />
    </svg>
  );
}

/** Fallback — generic terminal/bracket tile for unknown or custom agents. */
function CustomMark({ size, className }: MarkProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" fill="none" className={className} aria-hidden>
      <rect
        x="2"
        y="3"
        width="12"
        height="10"
        rx="2"
        stroke="currentColor"
        strokeWidth="1.2"
        fill="currentColor"
        fillOpacity="0.12"
      />
      <path
        d="M5 7l2 1.5L5 10"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path d="M9 10h2" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
    </svg>
  );
}
