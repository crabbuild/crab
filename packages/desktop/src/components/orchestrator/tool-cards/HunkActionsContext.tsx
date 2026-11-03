/**
 * Passes the active orchestrator task id down to tool cards so they can
 * perform hunk-level apply/revert without having it threaded through
 * every props boundary between the ChatTab and the Edit renderer.
 *
 * Rendered once at the root of the chat transcript; consumer cards read
 * it via `useHunkActions()`. When no provider is present (e.g. the tool
 * card is rendered in isolation) the hook returns `null`, which callers
 * should use to hide the hunk controls.
 */

import { createContext, useContext, type ReactNode } from "react";

export interface HunkActionsContextValue {
  taskId: string;
  /** Short label, used in toast notifications. */
  taskTitle?: string;
  /** Absolute path to the task's worktree on disk. Used by tool cards
   *  that need to resolve relative paths surfaced by the agent (e.g.
   *  ImageView, FilePreview). Optional because not every consumer of
   *  this context (chat-only views, snapshot rendering) has it. */
  worktreePath?: string;
}

const ctx = createContext<HunkActionsContextValue | null>(null);

export function HunkActionsProvider({
  value,
  children,
}: {
  value: HunkActionsContextValue;
  children: ReactNode;
}) {
  return <ctx.Provider value={value}>{children}</ctx.Provider>;
}

/** Read the active hunk actions context. Returns null when no provider is present. */
export function useHunkActions(): HunkActionsContextValue | null {
  return useContext(ctx);
}
