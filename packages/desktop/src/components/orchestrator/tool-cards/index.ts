/**
 * Tool Card Registry — agent-agnostic tool call rendering system.
 *
 * This module provides a registry of specialized renderers for
 * different tool types. The system is designed to be extensible:
 * new agents can be onboarded by mapping their tool names to
 * existing renderers or adding new ones.
 *
 * Architecture:
 *   1. Each tool type has a dedicated card component (EditCard,
 *      BashCard, etc.) that knows how to render that tool's
 *      input/output in a rich, informative way.
 *   2. The registry maps tool names → renderer components.
 *   3. `resolveToolCard()` looks up the right renderer for a
 *      given tool name, falling back to the generic ToolCard.
 *   4. New agents just need their adapter to emit standard
 *      ToolEvent objects — the renderer picks up automatically.
 *
 * To add support for a new agent's tools:
 *   - If the tool is semantically equivalent to an existing one
 *     (e.g., "file_write" ≈ "Write"), add the name to the
 *     existing registration in the registry below.
 *   - If it's a new tool type, create a new *Card.tsx component
 *     implementing ToolCardRendererProps and register it below.
 *
 * Coverage (Claude Code built-in tools):
 *   ✓ Agent           → SubAgentCard
 *   ✓ AskUserQuestion → AskUserCard (interactive: approve/reject/freeform)
 *   ✓ Bash            → BashCard
 *   ✓ CronCreate      → CronCard
 *   ✓ CronDelete      → CronCard
 *   ✓ CronList        → CronCard
 *   ✓ Edit            → EditCard
 *   ✓ EnterPlanMode   → PlanModeCard
 *   ✓ EnterWorktree   → WorktreeCard
 *   ✓ ExitPlanMode    → PlanModeCard
 *   ✓ ExitWorktree    → WorktreeCard
 *   ✓ Glob            → SearchCard
 *   ✓ Grep            → SearchCard
 *   ✓ ListMcpResources→ McpCard
 *   ✓ LSP             → LSPCard
 *   ✓ Monitor         → MonitorCard
 *   ✓ NotebookEdit    → EditCard
 *   ✓ PowerShell      → BashCard
 *   ✓ PushNotification→ NotificationCard
 *   ✓ Read            → ReadCard
 *   ✓ ReadMcpResource → McpCard
 *   ✓ RemoteTrigger   → NotificationCard
 *   ✓ SendMessage     → SubAgentCard
 *   ✓ ShareOnboardingGuide → SkillCard (generic plugin style)
 *   ✓ Skill           → SkillCard
 *   ✓ Task            → SubAgentCard (legacy)
 *   ✓ TaskCreate      → TaskManagementCard
 *   ✓ TaskGet         → TaskManagementCard
 *   ✓ TaskList        → TaskManagementCard
 *   ✓ TaskOutput      → SubAgentCard (legacy)
 *   ✓ TaskStop        → SubAgentCard (legacy, also TaskManagement)
 *   ✓ TaskUpdate      → TaskManagementCard
 *   ✓ TeamCreate      → SubAgentCard
 *   ✓ TeamDelete      → SubAgentCard
 *   ✓ TodoWrite       → TodoWriteCard (handled separately in dispatcher)
 *   ✓ ToolSearch      → SkillCard
 *   ✓ WebFetch        → WebFetchCard
 *   ✓ WebSearch       → WebFetchCard
 *   ✓ Write           → EditCard
 */

export type { ToolCardRendererProps, ToolCardRegistration } from "./types";
export { CollapsedCardLine } from "./CollapsedCardLine";
export type { CollapsedCardLineProps } from "./CollapsedCardLine";
export { EditCard } from "./EditCard";
export { BashCard } from "./BashCard";
export { ReadCard } from "./ReadCard";
export { SearchCard } from "./SearchCard";
export { WebFetchCard } from "./WebFetchCard";
export { SubAgentCard } from "./SubAgentCard";
export { PlanModeCard } from "./PlanCard";
export { AskUserCard } from "./AskUserCard";
export { MonitorCard } from "./MonitorCard";
export { LSPCard } from "./LSPCard";
export { SkillCard } from "./SkillCard";
export { CronCard } from "./CronCard";
export { WorktreeCard } from "./WorktreeCard";
export { TaskManagementCard } from "./TaskManagementCard";
export { McpCard } from "./McpCard";
export { NotificationCard } from "./NotificationCard";
export { CodexReviewCard, ContextCompactionCard } from "./CodexCard";
export { ApprovalCard } from "./ApprovalCard";
export { CommandSessionCardMemo as CommandSessionCard } from "./CommandSessionCard";
export { FileWatchNotificationCard } from "./FileWatchNotificationCard";
export { ImageViewCard } from "./ImageViewCard";

import type { ToolCardRendererProps } from "./types";
import { EditCard } from "./EditCard";
import { BashCard } from "./BashCard";
import { ReadCard } from "./ReadCard";
import { SearchCard } from "./SearchCard";
import { WebFetchCard } from "./WebFetchCard";
import { SubAgentCard } from "./SubAgentCard";
import { PlanModeCard } from "./PlanCard";
import { AskUserCard } from "./AskUserCard";
import { MonitorCard } from "./MonitorCard";
import { LSPCard } from "./LSPCard";
import { SkillCard } from "./SkillCard";
import { CronCard } from "./CronCard";
import { WorktreeCard } from "./WorktreeCard";
import { TaskManagementCard } from "./TaskManagementCard";
import { McpCard } from "./McpCard";
import { NotificationCard } from "./NotificationCard";
import { CodexReviewCard, ContextCompactionCard } from "./CodexCard";
import { ApprovalCard } from "./ApprovalCard";
import { ImageViewCard } from "./ImageViewCard";

// ── Registry ────────────────────────────────────────────────────────

/**
 * The tool card registry. Maps tool names to their specialized
 * renderer component. Order doesn't matter — lookup is by exact
 * tool name match.
 *
 * To onboard a new agent, add its tool names here. For example,
 * if a "Codex" agent uses "file_edit" instead of "Edit":
 *
 *   TOOL_CARD_REGISTRY.set("file_edit", EditCard);
 */
const TOOL_CARD_REGISTRY = new Map<string, React.ComponentType<ToolCardRendererProps>>();

// ─── File editing tools ─────────────────────────────────────────────
TOOL_CARD_REGISTRY.set("Edit", EditCard);
TOOL_CARD_REGISTRY.set("Write", EditCard);
TOOL_CARD_REGISTRY.set("MultiEdit", EditCard);
TOOL_CARD_REGISTRY.set("NotebookEdit", EditCard);
TOOL_CARD_REGISTRY.set("FileDelete", EditCard);

// ─── Shell / command execution ──────────────────────────────────────
TOOL_CARD_REGISTRY.set("Bash", BashCard);
TOOL_CARD_REGISTRY.set("Terminal", BashCard);
TOOL_CARD_REGISTRY.set("PowerShell", BashCard);

// ─── File reading ───────────────────────────────────────────────────
TOOL_CARD_REGISTRY.set("Read", ReadCard);

// ─── Search / file discovery ────────────────────────────────────────
TOOL_CARD_REGISTRY.set("Grep", SearchCard);
TOOL_CARD_REGISTRY.set("Glob", SearchCard);

// ─── Web ────────────────────────────────────────────────────────────
TOOL_CARD_REGISTRY.set("WebFetch", WebFetchCard);
TOOL_CARD_REGISTRY.set("WebSearch", WebFetchCard);

// ─── Planning ───────────────────────────────────────────────────────
TOOL_CARD_REGISTRY.set("EnterPlanMode", PlanModeCard);
TOOL_CARD_REGISTRY.set("ExitPlanMode", PlanModeCard);

// ─── Agent / sub-agent ──────────────────────────────────────────────
for (const tool of ["Agent", "SendMessage", "TeamCreate", "TeamDelete", "CodexAgent",
  "crab_spawn_agent", "crab_list_agents", "crab_send_message", "crab_read_agent", "crab_wait_agent", "crab_stop_agent"]) {
  TOOL_CARD_REGISTRY.set(tool, SubAgentCard);
}

// ─── Legacy sub-agent (Task/TaskOutput/TaskStop) ────────────────────
TOOL_CARD_REGISTRY.set("Task", SubAgentCard);
TOOL_CARD_REGISTRY.set("TaskOutput", SubAgentCard);
TOOL_CARD_REGISTRY.set("TaskStop", SubAgentCard);

// ─── Task management (new-style) ───────────────────────────────────
TOOL_CARD_REGISTRY.set("TaskCreate", TaskManagementCard);
TOOL_CARD_REGISTRY.set("TaskGet", TaskManagementCard);
TOOL_CARD_REGISTRY.set("TaskList", TaskManagementCard);
TOOL_CARD_REGISTRY.set("TaskUpdate", TaskManagementCard);

// ─── User interaction ───────────────────────────────────────────────
TOOL_CARD_REGISTRY.set("AskUserQuestion", AskUserCard);

// ─── Background monitoring ──────────────────────────────────────────
TOOL_CARD_REGISTRY.set("Monitor", MonitorCard);

// ─── Code intelligence (LSP) ────────────────────────────────────────
TOOL_CARD_REGISTRY.set("LSP", LSPCard);

// ─── Skills & tool discovery ────────────────────────────────────────
TOOL_CARD_REGISTRY.set("Skill", SkillCard);
TOOL_CARD_REGISTRY.set("ToolSearch", SkillCard);
TOOL_CARD_REGISTRY.set("ShareOnboardingGuide", SkillCard);

// ─── Scheduled tasks (Cron) ─────────────────────────────────────────
TOOL_CARD_REGISTRY.set("CronCreate", CronCard);
TOOL_CARD_REGISTRY.set("CronDelete", CronCard);
TOOL_CARD_REGISTRY.set("CronList", CronCard);

// ─── Worktree management ────────────────────────────────────────────
TOOL_CARD_REGISTRY.set("EnterWorktree", WorktreeCard);
TOOL_CARD_REGISTRY.set("ExitWorktree", WorktreeCard);

// ─── Notifications & remote triggers ────────────────────────────────
TOOL_CARD_REGISTRY.set("PushNotification", NotificationCard);
TOOL_CARD_REGISTRY.set("RemoteTrigger", NotificationCard);

// ─── MCP (Model Context Protocol) ──────────────────────────────────
TOOL_CARD_REGISTRY.set("ListMcpResources", McpCard);
TOOL_CARD_REGISTRY.set("ListMcpResourcesTool", McpCard);
TOOL_CARD_REGISTRY.set("ReadMcpResource", McpCard);
TOOL_CARD_REGISTRY.set("ReadMcpResourceTool", McpCard);

// ─── Codex / OpenAI agent tools (same semantics, different names) ───
TOOL_CARD_REGISTRY.set("file_edit", EditCard);
TOOL_CARD_REGISTRY.set("file_write", EditCard);
TOOL_CARD_REGISTRY.set("file_read", ReadCard);
TOOL_CARD_REGISTRY.set("file_change", EditCard);
TOOL_CARD_REGISTRY.set("shell", BashCard);
TOOL_CARD_REGISTRY.set("command_execution", BashCard);
TOOL_CARD_REGISTRY.set("web_search", WebFetchCard);
TOOL_CARD_REGISTRY.set("image_view", ImageViewCard);
TOOL_CARD_REGISTRY.set("ImageView", ImageViewCard);
TOOL_CARD_REGISTRY.set("todo_list", SubAgentCard);
TOOL_CARD_REGISTRY.set("dynamic_tool_call", BashCard);

// ─── Codex-specific status events ──────────────────────────────────

// ─── Command execution sessions (Codex command/exec protocol)

// ─── Filesystem watch notifications (Codex fs/watch protocol)
TOOL_CARD_REGISTRY.set("CodexReview", CodexReviewCard);
TOOL_CARD_REGISTRY.set("ContextCompaction", ContextCompactionCard);

// ─── Sandbox approval requests (Codex `approval_request` events) ───
TOOL_CARD_REGISTRY.set("CodexApproval", ApprovalCard);

// ─── Aider tools ────────────────────────────────────────────────────
TOOL_CARD_REGISTRY.set("replace_lines", EditCard);
TOOL_CARD_REGISTRY.set("insert_lines", EditCard);
TOOL_CARD_REGISTRY.set("search_replace", EditCard);

// ─── Cursor / Windsurf tools ────────────────────────────────────────
TOOL_CARD_REGISTRY.set("codebase_search", SearchCard);
TOOL_CARD_REGISTRY.set("read_file", ReadCard);
TOOL_CARD_REGISTRY.set("edit_file", EditCard);
TOOL_CARD_REGISTRY.set("run_terminal_command", BashCard);
TOOL_CARD_REGISTRY.set("list_dir", SearchCard);

// ── Lookup ──────────────────────────────────────────────────────────

/**
 * Resolve the specialized renderer for a tool name.
 * Returns undefined if no specialized renderer is registered,
 * signaling the caller should fall back to the generic ToolCard.
 */
export function resolveToolCard(
  toolName: string,
): React.ComponentType<ToolCardRendererProps> | undefined {
  return TOOL_CARD_REGISTRY.get(toolName.startsWith("mcp__crab_team__") ? toolName.slice("mcp__crab_team__".length) : toolName);
}

/**
 * Register a custom tool card renderer at runtime.
 * Useful for plugin-based agent onboarding.
 */
export function registerToolCard(
  toolName: string,
  component: React.ComponentType<ToolCardRendererProps>,
): void {
  TOOL_CARD_REGISTRY.set(toolName, component);
}

/**
 * Register multiple tool names to the same renderer.
 */
export function registerToolCards(
  toolNames: string[],
  component: React.ComponentType<ToolCardRendererProps>,
): void {
  for (const name of toolNames) {
    TOOL_CARD_REGISTRY.set(name, component);
  }
}

/**
 * Get all registered tool names (useful for debugging/introspection).
 */
export function getRegisteredTools(): string[] {
  return Array.from(TOOL_CARD_REGISTRY.keys());
}
