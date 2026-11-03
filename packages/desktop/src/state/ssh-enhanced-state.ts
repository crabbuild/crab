import type {
  TransferEntry,
  TransferStatus,
  SyncPair,
  SyncConflict,
  ConnectionQuality,
  RemoteBookmark,
  RecentFile,
  AgentCapabilities,
  RemoteProcess,
} from "../../electron/ssh-types";

// ── State Interfaces ────────────────────────────────────────────────

export interface TransferState {
  /** Active and recent transfers keyed by transfer ID */
  entries: Map<string, TransferEntry>;
  /** Currently selected transfer IDs (for batch operations) */
  selectedIds: Set<string>;
  /** Bandwidth limit in KB/s (0 = unlimited) */
  bandwidthLimit: number;
  /** Actual measured throughput in bytes/sec */
  actualThroughput: number;
  /** Whether compression is enabled */
  compressionEnabled: boolean;
  /** Whether adaptive throttling is enabled */
  adaptiveThrottling: boolean;
}

export interface SyncState {
  /** Sync pairs keyed by sync pair ID */
  pairs: Map<string, SyncPair>;
  /** Active conflicts keyed by conflict ID */
  conflicts: Map<string, SyncConflict>;
}

export interface QualityState {
  /** Connection quality per profile ID */
  profiles: Map<string, ConnectionQuality>;
}

export interface BookmarksState {
  /** Bookmarks keyed by profile ID */
  bookmarks: Map<string, RemoteBookmark[]>;
  /** Recent files keyed by profile ID */
  recents: Map<string, RecentFile[]>;
}

export interface CapabilitiesState {
  /** Agent capabilities keyed by profile ID */
  profiles: Map<string, AgentCapabilities>;
}

export interface ProcessesState {
  /** Process list keyed by profile ID */
  profiles: Map<string, RemoteProcess[]>;
  /** Whether polling is active per profile */
  polling: Map<string, boolean>;
}

export interface WorkspaceRemoteState {
  /** Currently open remote workspace path per profile */
  openPaths: Map<string, string>;
  /** Whether workspace indexing is in progress per profile */
  indexing: Map<string, boolean>;
}

export interface StreamEntry {
  id: string;
  profileId: string;
  type: "search" | "exec" | "tail" | "install";
  /** Accumulated output lines */
  output: string[];
  /** Whether the stream is still active */
  active: boolean;
  /** Exit code if stream has ended */
  exitCode?: number;
  /** Error message if stream failed */
  error?: string;
}

export interface StreamsState {
  /** Active streams keyed by stream ID */
  entries: Map<string, StreamEntry>;
}

export interface SshEnhancedState {
  transfers: TransferState;
  syncPairs: SyncState;
  quality: QualityState;
  bookmarks: BookmarksState;
  capabilities: CapabilitiesState;
  processes: ProcessesState;
  workspace: WorkspaceRemoteState;
  streams: StreamsState;
}

// ── Initial State ───────────────────────────────────────────────────

export const initialSshEnhancedState: SshEnhancedState = {
  transfers: {
    entries: new Map(),
    selectedIds: new Set(),
    bandwidthLimit: 0,
    actualThroughput: 0,
    compressionEnabled: false,
    adaptiveThrottling: false,
  },
  syncPairs: {
    pairs: new Map(),
    conflicts: new Map(),
  },
  quality: {
    profiles: new Map(),
  },
  bookmarks: {
    bookmarks: new Map(),
    recents: new Map(),
  },
  capabilities: {
    profiles: new Map(),
  },
  processes: {
    profiles: new Map(),
    polling: new Map(),
  },
  workspace: {
    openPaths: new Map(),
    indexing: new Map(),
  },
  streams: {
    entries: new Map(),
  },
};

// ── Action Types ────────────────────────────────────────────────────

export type SshEnhancedAction =
  // Transfer actions
  | { type: "TRANSFER_ADD"; entry: TransferEntry }
  | { type: "TRANSFER_UPDATE"; id: string; updates: Partial<TransferEntry> }
  | { type: "TRANSFER_REMOVE"; id: string }
  | { type: "TRANSFER_REMOVE_BATCH"; ids: string[] }
  | { type: "TRANSFER_CLEAR_COMPLETED" }
  | { type: "TRANSFER_SELECT"; id: string }
  | { type: "TRANSFER_DESELECT"; id: string }
  | { type: "TRANSFER_SELECT_BATCH"; ids: string[] }
  | { type: "TRANSFER_DESELECT_ALL" }
  | { type: "TRANSFER_SET_BANDWIDTH"; limit: number }
  | { type: "TRANSFER_SET_THROUGHPUT"; throughput: number }
  | { type: "TRANSFER_SET_COMPRESSION"; enabled: boolean }
  | { type: "TRANSFER_SET_ADAPTIVE"; enabled: boolean }
  | { type: "TRANSFER_PROGRESS"; id: string; bytesTransferred: number; speed: number; eta: number }
  | { type: "TRANSFER_STATUS_CHANGED"; id: string; status: TransferStatus; error?: string }
  // Sync actions
  | { type: "SYNC_SET_PAIR"; pair: SyncPair }
  | { type: "SYNC_REMOVE_PAIR"; id: string }
  | { type: "SYNC_UPDATE_PAIR"; id: string; updates: Partial<SyncPair> }
  | { type: "SYNC_ADD_CONFLICT"; conflict: SyncConflict }
  | { type: "SYNC_RESOLVE_CONFLICT"; id: string; resolution: "local" | "remote" | "both" }
  | { type: "SYNC_REMOVE_CONFLICT"; id: string }
  // Quality actions
  | { type: "QUALITY_UPDATE"; profileId: string; quality: ConnectionQuality }
  | { type: "QUALITY_REMOVE"; profileId: string }
  // Bookmarks actions
  | { type: "BOOKMARKS_SET"; profileId: string; bookmarks: RemoteBookmark[] }
  | { type: "BOOKMARKS_ADD"; bookmark: RemoteBookmark }
  | { type: "BOOKMARKS_REMOVE"; profileId: string; bookmarkId: string }
  | { type: "BOOKMARKS_SET_RECENTS"; profileId: string; recents: RecentFile[] }
  | { type: "BOOKMARKS_ADD_RECENT"; recent: RecentFile }
  // Capabilities actions
  | { type: "CAPABILITIES_SET"; profileId: string; capabilities: AgentCapabilities }
  | { type: "CAPABILITIES_REMOVE"; profileId: string }
  // Processes actions
  | { type: "PROCESSES_SET"; profileId: string; processes: RemoteProcess[] }
  | { type: "PROCESSES_CLEAR"; profileId: string }
  | { type: "PROCESSES_SET_POLLING"; profileId: string; active: boolean }
  // Workspace actions
  | { type: "WORKSPACE_OPEN"; profileId: string; remotePath: string }
  | { type: "WORKSPACE_CLOSE"; profileId: string }
  | { type: "WORKSPACE_SET_INDEXING"; profileId: string; indexing: boolean }
  // Stream actions
  | { type: "STREAM_START"; entry: StreamEntry }
  | { type: "STREAM_DATA"; streamId: string; data: string }
  | { type: "STREAM_END"; streamId: string; exitCode?: number; error?: string }
  | { type: "STREAM_REMOVE"; streamId: string };

// ── Reducer ─────────────────────────────────────────────────────────

/** Maximum output lines retained per stream to prevent unbounded memory growth. */
const MAX_STREAM_LINES = 1000;

export function sshEnhancedReducer(
  state: SshEnhancedState,
  action: SshEnhancedAction,
): SshEnhancedState {
  switch (action.type) {
    // ── Transfer reducers ──────────────────────────────────────────

    case "TRANSFER_ADD": {
      const entries = new Map(state.transfers.entries);
      entries.set(action.entry.id, action.entry);
      return { ...state, transfers: { ...state.transfers, entries } };
    }

    case "TRANSFER_UPDATE": {
      const existing = state.transfers.entries.get(action.id);
      if (!existing) return state;
      const entries = new Map(state.transfers.entries);
      entries.set(action.id, { ...existing, ...action.updates });
      return { ...state, transfers: { ...state.transfers, entries } };
    }

    case "TRANSFER_REMOVE": {
      const entries = new Map(state.transfers.entries);
      entries.delete(action.id);
      const selectedIds = new Set(state.transfers.selectedIds);
      selectedIds.delete(action.id);
      return { ...state, transfers: { ...state.transfers, entries, selectedIds } };
    }

    case "TRANSFER_REMOVE_BATCH": {
      const entries = new Map(state.transfers.entries);
      const selectedIds = new Set(state.transfers.selectedIds);
      for (const id of action.ids) {
        entries.delete(id);
        selectedIds.delete(id);
      }
      return { ...state, transfers: { ...state.transfers, entries, selectedIds } };
    }

    case "TRANSFER_CLEAR_COMPLETED": {
      const entries = new Map(state.transfers.entries);
      const selectedIds = new Set(state.transfers.selectedIds);
      for (const [id, entry] of entries) {
        if (entry.status === "completed" || entry.status === "failed") {
          entries.delete(id);
          selectedIds.delete(id);
        }
      }
      return { ...state, transfers: { ...state.transfers, entries, selectedIds } };
    }

    case "TRANSFER_SELECT": {
      const selectedIds = new Set(state.transfers.selectedIds);
      selectedIds.add(action.id);
      return { ...state, transfers: { ...state.transfers, selectedIds } };
    }

    case "TRANSFER_DESELECT": {
      const selectedIds = new Set(state.transfers.selectedIds);
      selectedIds.delete(action.id);
      return { ...state, transfers: { ...state.transfers, selectedIds } };
    }

    case "TRANSFER_SELECT_BATCH": {
      const selectedIds = new Set(state.transfers.selectedIds);
      for (const id of action.ids) {
        selectedIds.add(id);
      }
      return { ...state, transfers: { ...state.transfers, selectedIds } };
    }

    case "TRANSFER_DESELECT_ALL": {
      return { ...state, transfers: { ...state.transfers, selectedIds: new Set() } };
    }

    case "TRANSFER_SET_BANDWIDTH": {
      return { ...state, transfers: { ...state.transfers, bandwidthLimit: action.limit } };
    }

    case "TRANSFER_SET_THROUGHPUT": {
      return { ...state, transfers: { ...state.transfers, actualThroughput: action.throughput } };
    }

    case "TRANSFER_SET_COMPRESSION": {
      return { ...state, transfers: { ...state.transfers, compressionEnabled: action.enabled } };
    }

    case "TRANSFER_SET_ADAPTIVE": {
      return { ...state, transfers: { ...state.transfers, adaptiveThrottling: action.enabled } };
    }

    case "TRANSFER_PROGRESS": {
      const existing = state.transfers.entries.get(action.id);
      if (!existing) return state;
      const entries = new Map(state.transfers.entries);
      entries.set(action.id, {
        ...existing,
        bytesTransferred: action.bytesTransferred,
        speed: action.speed,
        eta: action.eta,
      });
      return { ...state, transfers: { ...state.transfers, entries } };
    }

    case "TRANSFER_STATUS_CHANGED": {
      const existing = state.transfers.entries.get(action.id);
      if (!existing) return state;
      const entries = new Map(state.transfers.entries);
      const updated: TransferEntry = { ...existing, status: action.status };
      if (action.error !== undefined) updated.error = action.error;
      if (action.status === "completed") updated.completedAt = Date.now();
      entries.set(action.id, updated);
      return { ...state, transfers: { ...state.transfers, entries } };
    }

    // ── Sync reducers ─────────────────────────────────────────────

    case "SYNC_SET_PAIR": {
      const pairs = new Map(state.syncPairs.pairs);
      pairs.set(action.pair.id, action.pair);
      return { ...state, syncPairs: { ...state.syncPairs, pairs } };
    }

    case "SYNC_REMOVE_PAIR": {
      const pairs = new Map(state.syncPairs.pairs);
      pairs.delete(action.id);
      // Also remove conflicts for this pair
      const conflicts = new Map(state.syncPairs.conflicts);
      for (const [cid, conflict] of conflicts) {
        if (conflict.syncPairId === action.id) {
          conflicts.delete(cid);
        }
      }
      return { ...state, syncPairs: { pairs, conflicts } };
    }

    case "SYNC_UPDATE_PAIR": {
      const existing = state.syncPairs.pairs.get(action.id);
      if (!existing) return state;
      const pairs = new Map(state.syncPairs.pairs);
      pairs.set(action.id, { ...existing, ...action.updates });
      return { ...state, syncPairs: { ...state.syncPairs, pairs } };
    }

    case "SYNC_ADD_CONFLICT": {
      const conflicts = new Map(state.syncPairs.conflicts);
      conflicts.set(action.conflict.id, action.conflict);
      return { ...state, syncPairs: { ...state.syncPairs, conflicts } };
    }

    case "SYNC_RESOLVE_CONFLICT": {
      const existing = state.syncPairs.conflicts.get(action.id);
      if (!existing) return state;
      const conflicts = new Map(state.syncPairs.conflicts);
      conflicts.set(action.id, {
        ...existing,
        resolvedAt: Date.now(),
        resolution: action.resolution,
      });
      return { ...state, syncPairs: { ...state.syncPairs, conflicts } };
    }

    case "SYNC_REMOVE_CONFLICT": {
      const conflicts = new Map(state.syncPairs.conflicts);
      conflicts.delete(action.id);
      return { ...state, syncPairs: { ...state.syncPairs, conflicts } };
    }

    // ── Quality reducers ──────────────────────────────────────────

    case "QUALITY_UPDATE": {
      const profiles = new Map(state.quality.profiles);
      profiles.set(action.profileId, action.quality);
      return { ...state, quality: { profiles } };
    }

    case "QUALITY_REMOVE": {
      const profiles = new Map(state.quality.profiles);
      profiles.delete(action.profileId);
      return { ...state, quality: { profiles } };
    }

    // ── Bookmarks reducers ────────────────────────────────────────

    case "BOOKMARKS_SET": {
      const bookmarks = new Map(state.bookmarks.bookmarks);
      bookmarks.set(action.profileId, action.bookmarks);
      return { ...state, bookmarks: { ...state.bookmarks, bookmarks } };
    }

    case "BOOKMARKS_ADD": {
      const bookmarks = new Map(state.bookmarks.bookmarks);
      const existing = bookmarks.get(action.bookmark.profileId) ?? [];
      bookmarks.set(action.bookmark.profileId, [...existing, action.bookmark]);
      return { ...state, bookmarks: { ...state.bookmarks, bookmarks } };
    }

    case "BOOKMARKS_REMOVE": {
      const bookmarks = new Map(state.bookmarks.bookmarks);
      const existing = bookmarks.get(action.profileId) ?? [];
      bookmarks.set(
        action.profileId,
        existing.filter((b) => b.id !== action.bookmarkId),
      );
      return { ...state, bookmarks: { ...state.bookmarks, bookmarks } };
    }

    case "BOOKMARKS_SET_RECENTS": {
      const recents = new Map(state.bookmarks.recents);
      recents.set(action.profileId, action.recents);
      return { ...state, bookmarks: { ...state.bookmarks, recents } };
    }

    case "BOOKMARKS_ADD_RECENT": {
      const recents = new Map(state.bookmarks.recents);
      const existing = recents.get(action.recent.profileId) ?? [];
      // Replace if same path exists, then trim to 20
      const filtered = existing.filter((r) => r.path !== action.recent.path);
      const updated = [action.recent, ...filtered].slice(0, 20);
      recents.set(action.recent.profileId, updated);
      return { ...state, bookmarks: { ...state.bookmarks, recents } };
    }

    // ── Capabilities reducers ─────────────────────────────────────

    case "CAPABILITIES_SET": {
      const profiles = new Map(state.capabilities.profiles);
      profiles.set(action.profileId, action.capabilities);
      return { ...state, capabilities: { profiles } };
    }

    case "CAPABILITIES_REMOVE": {
      const profiles = new Map(state.capabilities.profiles);
      profiles.delete(action.profileId);
      return { ...state, capabilities: { profiles } };
    }

    // ── Processes reducers ────────────────────────────────────────

    case "PROCESSES_SET": {
      const profiles = new Map(state.processes.profiles);
      profiles.set(action.profileId, action.processes);
      return { ...state, processes: { ...state.processes, profiles } };
    }

    case "PROCESSES_CLEAR": {
      const profiles = new Map(state.processes.profiles);
      profiles.delete(action.profileId);
      return { ...state, processes: { ...state.processes, profiles } };
    }

    case "PROCESSES_SET_POLLING": {
      const polling = new Map(state.processes.polling);
      polling.set(action.profileId, action.active);
      return { ...state, processes: { ...state.processes, polling } };
    }

    // ── Workspace reducers ────────────────────────────────────────

    case "WORKSPACE_OPEN": {
      const openPaths = new Map(state.workspace.openPaths);
      openPaths.set(action.profileId, action.remotePath);
      return { ...state, workspace: { ...state.workspace, openPaths } };
    }

    case "WORKSPACE_CLOSE": {
      const openPaths = new Map(state.workspace.openPaths);
      openPaths.delete(action.profileId);
      const indexing = new Map(state.workspace.indexing);
      indexing.delete(action.profileId);
      return { ...state, workspace: { openPaths, indexing } };
    }

    case "WORKSPACE_SET_INDEXING": {
      const indexing = new Map(state.workspace.indexing);
      indexing.set(action.profileId, action.indexing);
      return { ...state, workspace: { ...state.workspace, indexing } };
    }

    // ── Stream reducers ───────────────────────────────────────────

    case "STREAM_START": {
      const entries = new Map(state.streams.entries);
      entries.set(action.entry.id, action.entry);
      return { ...state, streams: { entries } };
    }

    case "STREAM_DATA": {
      const existing = state.streams.entries.get(action.streamId);
      if (!existing || !existing.active) return state;
      const entries = new Map(state.streams.entries);
      const output = [...existing.output, action.data];
      // Trim to MAX_STREAM_LINES to prevent unbounded growth
      if (output.length > MAX_STREAM_LINES) {
        output.splice(0, output.length - MAX_STREAM_LINES);
      }
      entries.set(action.streamId, { ...existing, output });
      return { ...state, streams: { entries } };
    }

    case "STREAM_END": {
      const existing = state.streams.entries.get(action.streamId);
      if (!existing) return state;
      const entries = new Map(state.streams.entries);
      entries.set(action.streamId, {
        ...existing,
        active: false,
        exitCode: action.exitCode,
        error: action.error,
      });
      return { ...state, streams: { entries } };
    }

    case "STREAM_REMOVE": {
      const entries = new Map(state.streams.entries);
      entries.delete(action.streamId);
      return { ...state, streams: { entries } };
    }

    default:
      return state;
  }
}
