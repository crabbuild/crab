/**
 * Terminal session persistence — layout serialization and deserialization.
 *
 * Handles saving and restoring terminal layouts (tabs, splits, active tab,
 * quick terminal) across app restarts. The layout is persisted to
 * the app profile by repository or task identity via IPC to the main process.
 */

import { createLeaf, deserializeTree, findLeaf, getAllLeaves, serializeTree, type LeafTab, type SplitNode } from "./terminal-split-tree";

// ── Types ───────────────────────────────────────────────────────────

export interface PersistedSession {
  id: string;
  shell: string;
  cwd: string;
  env?: Record<string, string>;
  startupCommand?: string;
  title: string;
  /** Path to serialized scrollback file (from SerializeAddon). */
  scrollbackPath?: string;
  sshRemote?: LeafTab["sshRemote"];
}

export interface PersistedLayout {
  version: number;
  tabs: PersistedSession[];
  splits: SplitArrangement[];
  activeTabIndex: number;
  quickTerminal?: PersistedSession;
  panes?: Array<{ id: string; tabIds: string[]; activeTab: string | null }>;
  activePaneId?: string;
}

export interface SplitArrangement {
  direction: "horizontal" | "vertical";
  sizes: number[];
  children: (string | SplitArrangement)[];
}

/** IPC request payload for saving terminal state. */
export interface SaveStateRequest {
  persistenceKey: string;
  layout: PersistedLayout;
  scrollbackData: Record<string, string>;
}

/** IPC response payload for restoring terminal state. */
export interface RestoreStateResponse {
  layout: PersistedLayout | null;
  scrollbackData: Record<string, string>;
}

/**
 * Renderer-side terminal view state used as input to serialization.
 * This mirrors the shape of the TerminalView component's state.
 */
export interface TerminalViewState {
  tabs: Array<{
    id: string;
    title: string;
    shell: string;
    cwd?: string;
    env?: Record<string, string>;
    startupCommand?: string;
  }>;
  splits: SplitArrangement[];
  activeTabIndex: number;
  quickTerminal?: {
    id: string;
    title: string;
    shell: string;
    cwd?: string;
    env?: Record<string, string>;
    startupCommand?: string;
  } | null;
}

// ── Current layout version ──────────────────────────────────────────

const LAYOUT_VERSION = 1;

// ── Serialization ───────────────────────────────────────────────────

/**
 * Convert the current terminal view state into a persistable layout.
 * Strips runtime-only fields and normalizes the structure.
 */
export function serializeLayout(state: TerminalViewState): PersistedLayout {
  const tabs: PersistedSession[] = state.tabs.map((tab) => ({
    id: tab.id,
    shell: tab.shell,
    cwd: tab.cwd ?? "",
    ...(tab.env && Object.keys(tab.env).length > 0 ? { env: tab.env } : {}),
    ...(tab.startupCommand ? { startupCommand: tab.startupCommand } : {}),
    title: tab.title,
  }));

  const layout: PersistedLayout = {
    version: LAYOUT_VERSION,
    tabs,
    splits: state.splits,
    activeTabIndex: Math.max(0, Math.min(state.activeTabIndex, tabs.length - 1)),
  };

  if (state.quickTerminal) {
    layout.quickTerminal = {
      id: state.quickTerminal.id,
      shell: state.quickTerminal.shell,
      cwd: state.quickTerminal.cwd ?? "",
      ...(state.quickTerminal.env && Object.keys(state.quickTerminal.env).length > 0
        ? { env: state.quickTerminal.env }
        : {}),
      ...(state.quickTerminal.startupCommand
        ? { startupCommand: state.quickTerminal.startupCommand }
        : {}),
      title: state.quickTerminal.title,
    };
  }

  return layout;
}

// ── Deserialization ─────────────────────────────────────────────────

/**
 * Parse and validate unknown data into a PersistedLayout.
 * Returns null if the data is invalid, corrupt, or an unsupported version.
 */
export function deserializeLayout(data: unknown): PersistedLayout | null {
  if (data === null || data === undefined || typeof data !== "object") {
    return null;
  }

  const obj = data as Record<string, unknown>;

  // Version check
  if (typeof obj.version !== "number" || obj.version !== LAYOUT_VERSION) {
    return null;
  }

  // Tabs validation
  if (!Array.isArray(obj.tabs)) {
    return null;
  }

  const tabs: PersistedSession[] = [];
  for (const tab of obj.tabs) {
    const session = validateSession(tab);
    if (!session) return null;
    tabs.push(session);
  }
  if (new Set(tabs.map(tab => tab.id)).size !== tabs.length) return null;

  // Splits validation
  if (!Array.isArray(obj.splits)) {
    return null;
  }

  const splits: SplitArrangement[] = [];
  for (const split of obj.splits) {
    const validated = validateSplitArrangement(split);
    if (!validated) return null;
    splits.push(validated);
  }

  // Active tab index validation
  if (typeof obj.activeTabIndex !== "number" || !Number.isFinite(obj.activeTabIndex)) {
    return null;
  }
  const activeTabIndex = Math.max(0, Math.min(Math.floor(obj.activeTabIndex), tabs.length - 1));

  let panes: PersistedLayout["panes"];
  if (obj.panes !== undefined) {
    if (!Array.isArray(obj.panes)) return null;
    const tabIds = new Set(tabs.map(tab => tab.id));
    const seen = new Set<string>();
    const paneIds = new Set<string>();
    panes = [];
    for (const pane of obj.panes) {
      if (!pane || typeof pane.id !== "string" || paneIds.has(pane.id) || !Array.isArray(pane.tabIds)) return null;
      paneIds.add(pane.id);
      for (const id of pane.tabIds) {
        if (typeof id !== "string" || !tabIds.has(id) || seen.has(id)) return null;
        seen.add(id);
      }
      if (pane.activeTab !== null && !pane.tabIds.includes(pane.activeTab)) return null;
      panes.push({ id: pane.id, tabIds: pane.tabIds, activeTab: pane.activeTab });
    }
    if (seen.size !== tabs.length || typeof obj.activePaneId !== "string" || !paneIds.has(obj.activePaneId)) return null;
    const references = (split: SplitArrangement): string[] => split.children.flatMap(child => typeof child === "string" ? [child] : references(child));
    const leafIds = splits.flatMap(references);
    if (panes.length === 1 ? splits.length !== 0 : splits.length !== 1) return null;
    if (panes.length > 1 && (leafIds.length !== paneIds.size || new Set(leafIds).size !== leafIds.length || leafIds.some(id => !paneIds.has(id)))) return null;
  }

  // Quick terminal (optional)
  let quickTerminal: PersistedSession | undefined;
  if (obj.quickTerminal !== undefined && obj.quickTerminal !== null) {
    const qt = validateSession(obj.quickTerminal);
    if (!qt) return null;
    quickTerminal = qt;
  }

  return {
    version: LAYOUT_VERSION,
    tabs,
    splits,
    activeTabIndex,
    ...(panes ? { panes, activePaneId: obj.activePaneId as string } : {}),
    ...(quickTerminal ? { quickTerminal } : {}),
  };
}

// ── Validation helpers ──────────────────────────────────────────────

function validateSession(data: unknown): PersistedSession | null {
  if (data === null || data === undefined || typeof data !== "object") {
    return null;
  }

  const obj = data as Record<string, unknown>;

  if (typeof obj.id !== "string" || obj.id.length === 0) return null;
  if (typeof obj.shell !== "string") return null;
  if (typeof obj.cwd !== "string") return null;
  if (typeof obj.title !== "string") return null;

  const session: PersistedSession = {
    id: obj.id,
    shell: obj.shell,
    cwd: obj.cwd,
    title: obj.title,
  };

  if (obj.env !== undefined && obj.env !== null) {
    if (typeof obj.env !== "object" || Array.isArray(obj.env)) return null;
    const env = obj.env as Record<string, unknown>;
    const validatedEnv: Record<string, string> = {};
    for (const [key, value] of Object.entries(env)) {
      if (typeof value !== "string") return null;
      validatedEnv[key] = value;
    }
    session.env = validatedEnv;
  }

  if (obj.startupCommand !== undefined && obj.startupCommand !== null) {
    if (typeof obj.startupCommand !== "string") return null;
    session.startupCommand = obj.startupCommand;
  }

  if (obj.scrollbackPath !== undefined && obj.scrollbackPath !== null) {
    if (typeof obj.scrollbackPath !== "string") return null;
    session.scrollbackPath = obj.scrollbackPath;
  }
  if (obj.sshRemote !== undefined) {
    if (!obj.sshRemote || typeof obj.sshRemote !== "object") return null;
    const remote = obj.sshRemote as Record<string, unknown>;
    if (typeof remote.profileId !== "string" || typeof remote.hostname !== "string") return null;
    if (remote.colour !== undefined && typeof remote.colour !== "string") return null;
    session.sshRemote = { profileId: remote.profileId, hostname: remote.hostname, ...(remote.colour ? { colour: remote.colour as string } : {}) };
  }

  return session;
}

function validateSplitArrangement(data: unknown): SplitArrangement | null {
  if (data === null || data === undefined || typeof data !== "object") {
    return null;
  }

  const obj = data as Record<string, unknown>;

  if (obj.direction !== "horizontal" && obj.direction !== "vertical") {
    return null;
  }

  if (!Array.isArray(obj.sizes)) return null;
  for (const size of obj.sizes) {
    if (typeof size !== "number" || !Number.isFinite(size) || size < 0) return null;
  }

  if (!Array.isArray(obj.children)) return null;
  if (obj.children.length !== obj.sizes.length) return null;
  if (obj.children.length < 2 || obj.sizes.reduce((total, size) => total + size, 0) <= 0) return null;

  const children: (string | SplitArrangement)[] = [];
  for (const child of obj.children) {
    if (typeof child === "string") {
      children.push(child);
    } else if (typeof child === "object" && child !== null) {
      const nested = validateSplitArrangement(child);
      if (!nested) return null;
      children.push(nested);
    } else {
      return null;
    }
  }

  return {
    direction: obj.direction,
    sizes: obj.sizes as number[],
    children,
  };
}

export function captureLayout(tree: SplitNode, activePaneId: string, cwd: (tab: LeafTab) => string): PersistedLayout {
  const leaves = getAllLeaves(tree);
  const tabs = leaves.flatMap(leaf => leaf.tabs);
  const active = findLeaf(tree, activePaneId) ?? leaves[0];
  const arrangement = serializeTree(tree);
  const layout = {
    ...serializeLayout({
      tabs: tabs.map(tab => ({ id: tab.id, title: tab.title, shell: tab.shell, cwd: cwd(tab) })),
      splits: arrangement ? [arrangement] : [],
      activeTabIndex: Math.max(0, tabs.findIndex(tab => tab.id === active?.activeTab)),
    }),
    panes: leaves.map(leaf => ({ id: leaf.id, tabIds: leaf.tabs.map(tab => tab.id), activeTab: leaf.activeTab })),
    activePaneId: active.id,
  };
  layout.tabs.forEach((tab, index) => { if (tabs[index].sshRemote) tab.sshRemote = tabs[index].sshRemote; });
  return layout;
}

export function restoreLayoutTree(layout: PersistedLayout, tabs: Map<string, LeafTab>): { tree: SplitNode; activePaneId: string } {
  if (!layout.panes) {
    const restored = layout.tabs.flatMap(tab => tabs.get(tab.id) ?? []);
    const active = tabs.get(layout.tabs[layout.activeTabIndex]?.id)?.id ?? restored[0]?.id ?? null;
    const tree = createLeaf(restored, active);
    return { tree, activePaneId: tree.id };
  }
  const leaves = new Map(layout.panes.map(pane => {
    const restored = pane.tabIds.flatMap(id => tabs.get(id) ?? []);
    return [pane.id, { tabs: restored, activeTab: tabs.get(pane.activeTab ?? "")?.id ?? restored[0]?.id ?? null }];
  }));
  const tree = deserializeTree(layout.splits[0] ?? null, leaves);
  const activePaneId = findLeaf(tree, layout.activePaneId ?? "")?.id ?? getAllLeaves(tree)[0].id;
  return { tree, activePaneId };
}
