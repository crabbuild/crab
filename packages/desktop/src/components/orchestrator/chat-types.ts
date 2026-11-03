/**
 * Shared types for the orchestrator chat system.
 *
 * Extracted from ChatTab.tsx to break the circular dependency:
 *   ChatTab → tool-cards/index → tool-cards/types → ChatTab
 */

export interface ChatMessage {
  id: string;
  role: "user" | "agent";
  content: string;
  timestamp: number;
  /** Semantic type for agent messages — prevents merging different kinds. */
  kind?: "text" | "thinking" | "error" | "tool" | "done";
  /** Inline tool-use events rendered within agent messages. */
  events?: ToolEvent[];
}

export interface ToolEvent {
  type: "file_read" | "file_edit" | "command" | "search" | "web" | "task" | "generic" | "thinking";
  /** The raw tool name from the adapter */
  tool?: string;
  summary: string;
  /** Full tool input object (for expanded view) */
  input?: unknown;
  /** Previous input state (for TodoWrite delta computation) */
  previousInput?: unknown;
  detail?: string;
  /** Codex item identifier — used by the AgentTasksView reducer to route
   *  streaming output deltas back to the card that owns this item. Set
   *  on tool_use events for items that emit subsequent output (Bash,
   *  Write, MCP, plan). */
  itemId?: string;
}

export interface Checkpoint {
  turnNumber: number;
  timestamp: number;
}
