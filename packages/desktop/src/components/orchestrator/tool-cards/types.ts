/**
 * Shared types for the tool card rendering system.
 *
 * This module defines the contract that all tool-specific renderers
 * implement. The system is agent-agnostic: any agent adapter that
 * emits ToolEvent objects with the standard fields will get rich
 * rendering automatically.
 */

import type { ReactNode } from "react";
import type { ToolEvent } from "../../../../electron/agent-chat-messages";
import type { CardId } from "../../../lib/turn-derivation";

/**
 * Props passed to every tool-specific card renderer.
 * Renderers can use whichever fields are relevant to their tool.
 */
export interface ToolCardRendererProps {
  /** The full ToolEvent from the adapter. */
  event: ToolEvent;
  /** Whether the card should start expanded. */
  defaultExpanded?: boolean;
  /** Whether the card is currently collapsed. */
  collapsed?: boolean;
  /** Callback to toggle the card's collapsed state. */
  onToggleCollapse?: (next: boolean) => void;
  /** Custom summary to show when the card is collapsed. */
  collapsedSummary?: ReactNode;
  /** Stable identity for this card instance. */
  cardId?: CardId;
}

/**
 * A tool card renderer registration entry.
 * Maps tool names (or patterns) to their specialized component.
 */
export interface ToolCardRegistration {
  /** Tool names this renderer handles (exact match). */
  tools: string[];
  /** The React component to render for these tools. */
  component: React.ComponentType<ToolCardRendererProps>;
}
