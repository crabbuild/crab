import { contextBridge, ipcRenderer, webUtils } from "electron";
import type { RpcReply } from "./rpc-transport";

// Whitelist of RPC method names the renderer is allowed to invoke.
// Any new agent verb must be added here; unknown calls are rejected
// before ever reaching the main process.
const ALLOWED_METHODS = new Set([
  "ping",
  "managed_profiles",
  "managed_select_profile",
  "managed_login",
  "managed_organizations",
  "managed_repositories",
  "managed_create_repository",
  "open_local",
  "open_url",
  "close",
  "auth_status",
  "list",
  "list_refs",
  "stat",
  "media_open",
  "media_close",
  "file_open",
  "file_chunks",
  "file_close",
  "file_materialize",
  "read_text",
  "read_bytes",
  "read_range",
  "preview_parquet",
  "preview_safetensors",
  "preview_npy",
  "preview_npz",
  "preview_zip",
  "repo_summary",
  "git_status_porcelain",
  "file_diff",
  "git_grep",
  "search_replace",
  "hydration_status",
  "git_log",
  "git_stash_list",
  "git_stash_apply",
  "git_stash_pop",
  "git_stash_drop",
  "git_stash_save",
  "git_stash_show",
  "git_stash_show_patch",
  "pointer_top",
  "git_commit_changes",
  "resolve_ref",
  "about",
  "agent_health",
  "cache_stats",
  "cache_clean",
  "clone",
  "status",
  "hydrate",
  "dehydrate",
  "push",
  "add",
  // Hunk staging
  "git_diff_index_patch",
  "git_diff_cached_patch",
  "git_apply_patch",
  "git_stage_hunks",
  "git_unstage_hunks",
  "git_discard_hunks",
  "git_add_paths",
  "git_reset_paths",
  // Commit
  "git_commit",
  "git_config_get",
  // Branches
  "git_checkout",
  "git_branch_create",
  "git_branch_delete",
  "git_branch_rename",
  "git_set_upstream",
  // Per-branch push/pull/fetch
  "git_fetch_branch",
  "git_pull_branch",
  "git_push_branch",
  "git_fetch_all",
  "git_pull_mode",
  "git_push_remote",
  // Merge/Rebase/Cherry-pick
  "op_start",
  "op_continue",
  "op_abort",
  "op_status",
  "op_edit_todo",
  // Conflict resolution
  "conflict_list",
  "conflict_read_sides",
  "git_apply_resolution",
  // Legacy verbs (kept for back-compat)
  "git_tag_add",
  "git_tag_delete",
  "git_cherry_pick",
  "git_revert",
  "git_merge",
  "git_rebase",
  "git_reset",
  "git_fetch",
  "git_pull",
  "git_activity",
  "git_contributors",
  "git_pulse",
  "git_commit_frequency",
  "git_code_frequency",
  "git_branch_health",
  "git_release_timeline",
  "git_commit_size_distribution",
  "git_contributor_growth",
  "repo_size_history",
  "dedup_savings",
  "preview_sqlite",
  "duckdb_query",
  "preview_onnx",
  "preview_hdf5",
  "preview_protobuf",
  "preview_stl",
  "git_branch_info",
  "git_repo_info",
  // Experiment tracking
  "experiments_runs_for_commit",
  "experiments_runs_recent",
  "experiments_run_detail",
  "experiments_configure",
  // Notebook
  "preview_notebook_meta",
  "notebook_strip_outputs",
  // Large-file pre-commit
  "crab_add_auto",
  "crab_track_and_add",
  // Additional previews
  "preview_torch_checkpoint",
  // Storage pagination
  "storage_summary",
  // Indexer (I.2)
  "indexer_status",
  // Cancellable RPCs
  "rpc/cancel",
  // Forge auth
  "forge_auth_start",
  // Forge / Pull Requests
  "forge_list_prs",
  "forge_create_pr",
  "forge_pr_detail",
  "forge_pr_comment",
  "forge_submit_review",
  "forge_list_checks",
  "forge_list_issues",
  // Crab flow integration — structured add/push/staging/storage
  "crab_add_structured",
  "crab_push_structured",
  "staging_stats",
  "staging_clean",
  "crab_reset_files",
  "crab_init",
  "storage_config_get",
  "storage_config_verify",
  "storage_connections_list",
  "storage_connections_save",
  "storage_connections_remove",
  "storage_connections_test",
  "storage_connections_bind",
  // Workflow engine — visual DAG editor
  "workflow_list",
  "workflow_get",
  "workflow_save",
  "workflow_create",
  "workflow_delete",
  "workflow_run",
  "workflow_cancel",
  "workflow_validate",
  "workflow_status",
  "workflow_parse",
  "workflow_dag",
  "workflow_resolve_template",
  "workflow_template_completions",
  "workflow_exp_list",
  "workflow_exp_enqueue",
  "workflow_exp_start",
  "workflow_exp_stop",
  "workflow_migrate_preview",
  "workflow_migrate_apply",
  // File write operations
  "write_text",
  "write_bytes",
  // Theme management
  "save_imported_theme",
  // Git worktree management
  "git_worktree_list",
  "git_worktree_add",
  "git_worktree_remove",
  "git_worktree_move",
  "git_worktree_prune",
  "git_worktree_lock",
  "git_worktree_unlock",
  "git_worktree_repair",
  // Worktree manager (hydrated metadata layer)
  "worktree_list",
  "worktree_status",
  "worktree_set_meta",
  "worktree_push_estimate",
  "worktree_repair",
  // Cross-worktree comparison
  "git_diff_name_only",
  "git_blame",
  // Remote management
  "git_remote_add",
  "git_remote_remove",
  "git_remote_set_url",
  // SSH remote connections
  "ssh_list_profiles",
  "ssh_save_profile",
  "ssh_delete_profile",
  "ssh_connect",
  "ssh_disconnect",
  "ssh_test_connection",
  "ssh_get_status",
  // SSH remote terminal
  "ssh_open_terminal",
  "ssh_close_terminal",
  "ssh_terminal_resize",
  "ssh_terminal_write",
  "ssh_terminal_reconnect",
  // SSH port forwarding
  "ssh_add_forward",
  "ssh_remove_forward",
  "ssh_list_forwards",
  "ssh_open_forward_browser",
  // SSH remote file browsing
  "ssh_browse_path",
  "ssh_home_dir",
  // SSH remote file editing
  "ssh_read_file",
  "ssh_write_file",
  "ssh_create_file",
  "ssh_delete_file",
  "ssh_rename_file",
]);

type Listener = (event: { method: string; params: unknown }) => void;

const listeners = new Set<Listener>();
ipcRenderer.on("crab:event", (_e, payload: { method: string; params: unknown }) => {
  for (const l of listeners) l(payload);
});

contextBridge.exposeInMainWorld("crab", {
  rpc: async (method: string, params: unknown): Promise<unknown> => {
    if (!ALLOWED_METHODS.has(method)) {
      throw new Error(`method not allowed: ${method}`);
    }
    const reply: RpcReply<unknown> = await ipcRenderer.invoke("crab:rpc", method, params);
    // Promise rejection with a plain value preserves code/data through the
    // context bridge; throwing an Error here would discard custom properties.
    if (!reply.ok) return Promise.reject(reply.error);
    return reply.value;
  },
  pickRepoDirectory: () => ipcRenderer.invoke("crab:dialog-open-repo"),
  pickCloneDestination: () => ipcRenderer.invoke("crab:dialog-select-destination"),
  pickWorktreePath: () => ipcRenderer.invoke("crab:dialog-pick-worktree-path"),
  pickJsonFile: (title?: string) => ipcRenderer.invoke("crab:dialog-pick-json-file", title),
  updateMainSettings: (u: Record<string, unknown>) =>
    ipcRenderer.invoke("crab:main-settings-update", u),
  setDevtools: (open: boolean) => ipcRenderer.invoke("crab:set-devtools", open),
  getNoTelemetryFlag: () => ipcRenderer.invoke("crab:no-telemetry-flag") as Promise<boolean>,
  openLogsFolder: () => ipcRenderer.invoke("crab:open-logs-folder"),
  // File system operations
  fsReveal: (fullPath: string) => ipcRenderer.invoke("crab:fs-reveal", fullPath),
  // Overloaded: fsCreate(fullPath, kind) or fsCreate(repoId, fullPath, kind)
  fsCreate: (...args: [string, "file" | "directory"] | [string, string, "file" | "directory"]) => {
    if (args.length === 3) {
      // New overload: (repoId, fullPath, kind)
      return ipcRenderer.invoke("crab:fs-create", args[0], args[1], args[2]);
    }
    // Legacy: (fullPath, kind)
    return ipcRenderer.invoke("crab:fs-create", args[0], args[1]);
  },
  // Overloaded: fsRename(oldPath, newPath) or fsRename(repoId, oldPath, newPath)
  fsRename: (...args: [string, string] | [string, string, string]) => {
    if (args.length === 3) {
      // New overload: (repoId, oldPath, newPath)
      return ipcRenderer.invoke("crab:fs-rename", args[0], args[1], args[2]);
    }
    // Legacy: (oldPath, newPath)
    return ipcRenderer.invoke("crab:fs-rename", args[0], args[1]);
  },
  // Overloaded: fsDelete(fullPath, useTrash?) or fsDelete(repoId, fullPath, useTrash?)
  fsDelete: (...args: [string, boolean?] | [string, string, boolean?]) => {
    if (args.length >= 2 && typeof args[1] === "string") {
      // New overload: (repoId, fullPath, useTrash?)
      const useTrash = (args as [string, string, boolean?])[2] ?? true;
      return ipcRenderer.invoke("crab:fs-delete", args[0], args[1], useTrash);
    }
    // Legacy: (fullPath, useTrash?)
    const useTrash = (args as [string, boolean?])[1] ?? true;
    return ipcRenderer.invoke("crab:fs-delete", args[0], useTrash);
  },
  fsOpenExternal: (fullPath: string) =>
    ipcRenderer.invoke("crab:fs-open-external", fullPath),
  fsWriteFile: (repoId: string, fullPath: string, content: string, nativeSessionId: string | null) =>
    ipcRenderer.invoke("crab:fs-write-file", repoId, fullPath, content, nativeSessionId),
  fsReadFile: (...args: [string] | [string, string, number | undefined, string | null]) =>
    ipcRenderer.invoke("crab:fs-read-file", ...args) as Promise<{ exists: boolean; content: string | null }>,
  fsReadImage: (fullPath: string) =>
    ipcRenderer.invoke("crab:fs-read-image", fullPath) as Promise<{
      exists: boolean;
      dataUrl: string | null;
      mimeType: string | null;
      bytes: number;
      tooLarge?: boolean;
    }>,
  // File system watcher
  fsWatchStart: (repoPath: string) =>
    ipcRenderer.invoke("crab:fs-watch-start", repoPath) as Promise<{ watching: boolean; path: string }>,
  fsWatchStop: (repoPath: string) =>
    ipcRenderer.invoke("crab:fs-watch-stop", repoPath) as Promise<{ watching: boolean; path: string }>,
  // Repo capabilities
  getCapabilities: (repoId: string) =>
    ipcRenderer.invoke("crab:repo-get-capabilities", repoId) as Promise<{
      canEdit: boolean;
      canWatch: boolean;
      canStageHunks: boolean;
      canWorktree: boolean;
      canForge: boolean;
      canHydrate: boolean;
      canSearch: boolean;
      canExec: boolean;
      canTrash: boolean;
    }>,
  // Dialogs (prompt/confirm replacements for sandboxed renderer)
  dialogPrompt: (message: string, defaultValue?: string) =>
    ipcRenderer.invoke("crab:dialog-prompt", message, defaultValue) as Promise<string | null>,
  dialogConfirm: (message: string) =>
    ipcRenderer.invoke("crab:dialog-confirm", message) as Promise<boolean>,
  // Terminal
  //
  // New object-based API:
  //   termSpawn({ cwd, sessionType?, shell?, env?, startupCommand?, agentMeta? })
  //   → Promise<{ id: string; resolvedSessionType: "daemon" | "in-process" }>
  //
  // Legacy positional shim (kept until all call sites migrate):
  //   termSpawn(cwd, shell?, extraEnv?, startupCommand?)
  //   → same return type
  termSpawn: (
    cwdOrOpts:
      | string
      | {
          cwd: string;
          ownerPath?: string;
          sessionType?: "daemon" | "in-process";
          shell?: string;
          env?: Record<string, string>;
          startupCommand?: string;
          agentMeta?: { taskId: string; agentType: string; worktreePath: string };
        },
    shell?: string,
    extraEnv?: Record<string, string>,
    startupCommand?: string,
  ): Promise<{ id: string; resolvedSessionType: "daemon" | "in-process" }> => {
    const payload = typeof cwdOrOpts === "string"
      ? { cwd: cwdOrOpts, shell, env: extraEnv, startupCommand }
      : cwdOrOpts;
    return ipcRenderer.invoke("crab:term-spawn", payload).then(result => {
      if (result?.ok === false) throw new Error(result.message);
      return result;
    });
  },
  termWrite: (id: string, data: string) => ipcRenderer.invoke("crab:term-write", id, data),
  termResize: (id: string, cols: number, rows: number) => ipcRenderer.invoke("crab:term-resize", id, cols, rows),
  termKill: (id: string) => ipcRenderer.invoke("crab:term-kill", id),
  termListShells: () => ipcRenderer.invoke("crab:term-list-shells") as Promise<string[]>,
  termPoolHint: (cwd: string) => ipcRenderer.invoke("crab:term-pool-hint", cwd),
  termSaveState: (state: { persistenceKey: string; layout: unknown; scrollbackData: Record<string, string> }) =>
    ipcRenderer.invoke("crab:term-save-state", state) as Promise<{ success: boolean; error?: string }>,
  termRestoreState: (persistenceKey: string) =>
    ipcRenderer.invoke("crab:term-restore-state", persistenceKey) as Promise<{ layout: unknown | null; scrollbackData: Record<string, string> }>,
  termNotify: (req: { title: string; body: string; icon: "success" | "error"; terminalId: string; tabIndex: number }) =>
    ipcRenderer.invoke("crab:term-notify", req),
  termLogPath: (sessionId: string) =>
    ipcRenderer.invoke("crab:term-log-path", sessionId) as Promise<{ path: string; exists: boolean; sizeBytes: number }>,
  termLogRead: (sessionId: string) =>
    ipcRenderer.invoke("crab:term-log-read", sessionId) as Promise<{ content: string | null; error: string | null }>,
  termLogList: () =>
    ipcRenderer.invoke("crab:term-log-list") as Promise<Array<{ sessionId: string; path: string; sizeBytes: number; mtimeMs: number }>>,
  termClearSession: (sessionId: string, persistenceKey: string) =>
    ipcRenderer.invoke("crab:term-clear-session", sessionId, persistenceKey) as Promise<{ success: boolean }>,
  termListDaemonSessions: (repoPath: string) =>
    ipcRenderer.invoke("crab:term-list-daemon-sessions", { repoPath }) as Promise<Array<{
      id: string;
      daemonSessionId: string;
      state: string;
      shellState?: string;
      isAlive: boolean;
      pid: number | null;
      initialCwd?: string | null;
      cwd: string | null;
      cols: number;
      rows: number;
      createdAt: number;
      lastDataAt: number;
      agentMeta?: {
        taskId: string;
        agentType: string;
        worktreePath: string;
        activityState?: string;
        stateChangedAt?: number;
      };
    }>>,
  termAttachDaemonSession: (daemonSessionId: string, repoPath: string) =>
    ipcRenderer.invoke("crab:term-attach-daemon-session", { daemonSessionId, repoPath }) as Promise<{
      id: string;
      daemonSessionId: string;
      snapshot: string | null;
      cwd: string | null;
      summary: {
        id: string;
        daemonSessionId: string;
        state: string;
        shellState?: string;
        isAlive: boolean;
        pid: number | null;
        initialCwd?: string | null;
        cwd: string | null;
        cols: number;
        rows: number;
        createdAt: number;
        lastDataAt: number;
        agentMeta?: {
          taskId: string;
          agentType: string;
          worktreePath: string;
          activityState?: string;
          stateChangedAt?: number;
        };
      };
    }>,
  termKillDaemonSession: (daemonSessionId: string, repoPath: string) =>
    ipcRenderer.invoke("crab:term-kill-daemon-session", { daemonSessionId, repoPath }) as Promise<void>,
  termDetachDaemonSession: (rendererId: string) =>
    ipcRenderer.invoke("crab:term-detach-daemon-session", { rendererId }) as Promise<void>,
  // Reattach to existing daemon sessions for a given repo path.
  // Called by TerminalView on mount to reconnect to persistent sessions.
  termReattachSessions: (repoPath: string) =>
    ipcRenderer.invoke("crab:term-reattach-sessions", { repoPath }) as Promise<Array<{ id: string; snapshot: string | null; cwd: string | null }>>,
  // PTY mode — resolved at startup by mode-selection.ts
  getPtyMode: () =>
    ipcRenderer.invoke("crab:pty-mode") as Promise<{
      resolvedMode: "daemon" | "in-process";
      configuredMode: "auto" | "daemon" | "in-process";
      fallback: { reason: string; error?: string; timestamp: number } | null;
    }>,
  // Subscribe to live PTY mode updates pushed from the main process.
  // Fires when the daemon connects, disconnects, or the circuit breaker
  // changes state. Returns an unsubscribe function.
  onPtyMode: (listener: (mode: { daemonReady: boolean; breakerOpen: boolean }) => void) => {
    const handler = (_e: unknown, payload: { daemonReady: boolean; breakerOpen: boolean }) =>
      listener(payload);
    ipcRenderer.on("crab:pty-mode-changed", handler);
    return () => { ipcRenderer.removeListener("crab:pty-mode-changed", handler); };
  },
  // PTY daemon diagnostics and upgrade
  daemonDiagnostics: () =>
    ipcRenderer.invoke("crab:daemon-diagnostics") as Promise<unknown>,
  daemonRestart: () =>
    ipcRenderer.invoke("crab:daemon-restart") as Promise<{ success: boolean; killedSessionCount: number; error?: string }>,
  daemonPrepareUpgrade: (opts?: { successorPath?: string }) =>
    ipcRenderer.invoke("crab:daemon-prepare-upgrade", opts) as Promise<{ success: boolean; error?: string }>,
  onTermLogError: (listener: (evt: { sessionId: string; error: string }) => void) => {
    const handler = (_e: unknown, payload: { sessionId: string; error: string }) => listener(payload);
    ipcRenderer.on("crab:term-log-error", handler);
    return () => { ipcRenderer.removeListener("crab:term-log-error", handler); };
  },
  onTermActivateTab: (listener: (evt: { terminalId: string; tabIndex: number }) => void) => {
    const handler = (_e: unknown, payload: { terminalId: string; tabIndex: number }) => listener(payload);
    ipcRenderer.on("crab:term-activate-tab", handler);
    return () => { ipcRenderer.removeListener("crab:term-activate-tab", handler); };
  },
  onTermData: (listener: (evt: { id: string; data: string }) => void) => {
    const handler = (_e: unknown, payload: { id: string; data: string }) => listener(payload);
    ipcRenderer.on("crab:term-data", handler);
    return () => { ipcRenderer.removeListener("crab:term-data", handler); };
  },
  onTermExit: (listener: (evt: { id: string; exitCode: number; reason?: string }) => void) => {
    const handler = (_e: unknown, payload: { id: string; exitCode: number; reason?: string }) => listener(payload);
    ipcRenderer.on("crab:term-exit", handler);
    return () => { ipcRenderer.removeListener("crab:term-exit", handler); };
  },
  onTermSaveRequest: (listener: () => void) => {
    const handler = () => listener();
    ipcRenderer.on("crab:term-save-request", handler);
    return () => { ipcRenderer.removeListener("crab:term-save-request", handler); };
  },
  // Cold restore — emitted by the daemon bridge when a session can be resumed
  // after a crash. The renderer shows a confirm dialog and calls confirm/discard.
  onTermColdRestore: (listener: (evt: {
    id: string;
    savedAt: number;
    sanitizedKeys: string[];
    cwd: string | null;
    cols: number;
    rows: number;
  }) => void) => {
    const handler = (_e: unknown, payload: {
      id: string;
      savedAt: number;
      sanitizedKeys: string[];
      cwd: string | null;
      cols: number;
      rows: number;
    }) => listener(payload);
    ipcRenderer.on("crab:term-cold-restore", handler);
    return () => { ipcRenderer.removeListener("crab:term-cold-restore", handler); };
  },
  termColdRestoreConfirm: (sessionId: string, env: Record<string, string>) =>
    ipcRenderer.invoke("crab:term-cold-restore-confirm", { sessionId, env }) as Promise<void>,
  termColdRestoreDiscard: (sessionId: string) =>
    ipcRenderer.invoke("crab:term-cold-restore-discard", { sessionId }) as Promise<void>,
  // Agent dashboard
  agentSessions: () =>
    ipcRenderer.invoke("crab:agent-sessions") as Promise<Array<{
      sessionId: string;
      taskId: string;
      agentType: string;
      worktreePath: string;
      state: string;
      stateChangedAt: number;
      elapsedWorkingMs: number;
      exitCode?: number;
    }>>,
  onAgentStateChange: (listener: (evt: { sessionId: string; state: string; previousState: string; timestamp: number }) => void) => {
    const handler = (_e: unknown, payload: { sessionId: string; state: string; previousState: string; timestamp: number }) => listener(payload);
    ipcRenderer.on("crab:agent-state-changed", handler);
    return () => { ipcRenderer.removeListener("crab:agent-state-changed", handler); };
  },
  // Open a URL in the system browser
  openUrl: (url: string) => ipcRenderer.invoke("crab:open-url", url),
  // Jupyter kernel
  jupyterListKernels: (pythonPath: string) => ipcRenderer.invoke("crab:jupyter-list-kernels", pythonPath),
  jupyterCheck: (pythonPath?: string) => ipcRenderer.invoke("crab:jupyter-check", pythonPath),
  jupyterInstall: (pythonPath: string) => ipcRenderer.invoke("crab:jupyter-install", pythonPath),
  jupyterStart: (kernelName: string, repoId: string, nativeSessionId: string, pythonPath: string) => ipcRenderer.invoke("crab:jupyter-start", kernelName, repoId, nativeSessionId, pythonPath),
  jupyterExecute: (id: string, code: string, requestId: string) => ipcRenderer.invoke("crab:jupyter-execute", id, code, requestId),
  jupyterInterrupt: (id: string) => ipcRenderer.invoke("crab:jupyter-interrupt", id),
  jupyterRestart: (id: string) => ipcRenderer.invoke("crab:jupyter-restart", id),
  jupyterShutdown: (id: string) => ipcRenderer.invoke("crab:jupyter-shutdown", id),
  onJupyterOutput: (listener: (evt: import("./jupyter-types").KernelOutput & { id: string }) => void) => {
    const handler = (_e: unknown, payload: any) => listener(payload);
    ipcRenderer.on("crab:jupyter-output", handler);
    return () => { ipcRenderer.removeListener("crab:jupyter-output", handler); };
  },
  onJupyterStatus: (listener: (evt: { id: string; status: string }) => void) => {
    const handler = (_e: unknown, payload: any) => listener(payload);
    ipcRenderer.on("crab:jupyter-status", handler);
    return () => { ipcRenderer.removeListener("crab:jupyter-status", handler); };
  },
  // Sidecar runners
  sidecarStart: (spec: unknown) => ipcRenderer.invoke("sidecar:start", spec),
  sidecarStop: (id: string) => ipcRenderer.invoke("sidecar:stop", id),
  sidecarStatus: (id?: string) => ipcRenderer.invoke("sidecar:status", id),
  sidecarLogs: (id: string) => ipcRenderer.invoke("sidecar:logs", id),
  // Keychain (secure credential storage)
  // Full keytar-style API: service + account parameters
  keychain: {
    getPassword: (service: string, account: string) =>
      ipcRenderer.invoke("crab:keychain-get-password", service, account) as Promise<string | null>,
    setPassword: (service: string, account: string, password: string) =>
      ipcRenderer.invoke("crab:keychain-set-password", service, account, password),
    deletePassword: (service: string, account: string) =>
      ipcRenderer.invoke("crab:keychain-delete-password", service, account) as Promise<boolean>,
    // Simplified single-key API (uses "crab-desktop" as service)
    set: (key: string, value: string) => ipcRenderer.invoke("crab:keychain-set", key, value),
    get: (key: string) => ipcRenderer.invoke("crab:keychain-get", key) as Promise<string | null>,
    delete: (key: string) => ipcRenderer.invoke("crab:keychain-delete", key),
    has: (key: string) => ipcRenderer.invoke("crab:keychain-has", key) as Promise<boolean>,
  },
  onEvent: (listener: Listener) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  },

  // ─── SSH Remote Connections ─────────────────────────────────────────
  ssh: {
    answerHostKey: (requestId: string, accepted: boolean) => ipcRenderer.invoke("ssh:host-key-answer", requestId, accepted) as Promise<void>,
    onHostKeyRequest: (listener: (payload: import("./ssh-known-hosts").HostKeyRequest) => void) => {
      const handler = (_event: unknown, payload: import("./ssh-known-hosts").HostKeyRequest) => listener(payload);
      ipcRenderer.on("ssh:host-key-request", handler);
      return () => { ipcRenderer.removeListener("ssh:host-key-request", handler); };
    },
    onHostKeyClosed: (listener: (payload: { requestId: string }) => void) => {
      const handler = (_event: unknown, payload: { requestId: string }) => listener(payload);
      ipcRenderer.on("ssh:host-key-closed", handler);
      return () => { ipcRenderer.removeListener("ssh:host-key-closed", handler); };
    },
    listProfiles: () =>
      ipcRenderer.invoke("ssh:list-profiles"),
    saveProfile: (profile: Record<string, unknown>) =>
      ipcRenderer.invoke("ssh:save-profile", profile),
    deleteProfile: (profileId: string) =>
      ipcRenderer.invoke("ssh:delete-profile", profileId),
    connect: (profileId: string) =>
      ipcRenderer.invoke("ssh:connect", profileId),
    disconnect: (profileId: string) =>
      ipcRenderer.invoke("ssh:disconnect", profileId),
    testConnection: (profileId: string) =>
      ipcRenderer.invoke("ssh:test-connection", profileId),
    getStatus: (profileId: string) =>
      ipcRenderer.invoke("ssh:get-status", profileId),
    // Remote terminal
    openTerminal: (profileId: string, cols: number, rows: number, cwd?: string, resumeId?: string, ownerPath?: string) =>
      ipcRenderer.invoke("ssh:open-terminal", profileId, cols, rows, cwd, resumeId, ownerPath) as Promise<{ terminalId: string }>,
    closeTerminal: (terminalId: string) =>
      ipcRenderer.invoke("ssh:close-terminal", terminalId),
    detachTerminal: (terminalId: string) => ipcRenderer.invoke("ssh:detach-terminal", terminalId),
    terminalResize: (terminalId: string, cols: number, rows: number) =>
      ipcRenderer.invoke("ssh:terminal-resize", terminalId, cols, rows),
    terminalWrite: (terminalId: string, data: string) =>
      ipcRenderer.invoke("ssh:terminal-write", terminalId, data),
    terminalReconnect: (terminalId: string) =>
      ipcRenderer.invoke("ssh:terminal-reconnect", terminalId) as Promise<string>,
    onTerminalData: (listener: (evt: { terminalId: string; data: string }) => void) => {
      const handler = (_e: unknown, payload: { terminalId: string; data: string }) => listener(payload);
      ipcRenderer.on("ssh:terminal-data", handler);
      return () => { ipcRenderer.removeListener("ssh:terminal-data", handler); };
    },
    onTerminalClosed: (listener: (evt: { terminalId: string; profileId: string }) => void) => {
      const handler = (_e: unknown, payload: { terminalId: string; profileId: string }) => listener(payload);
      ipcRenderer.on("ssh:terminal-closed", handler);
      return () => { ipcRenderer.removeListener("ssh:terminal-closed", handler); };
    },
    onTerminalSessionEnded: (listener: (evt: { terminalId: string; profileId: string }) => void) => {
      const handler = (_e: unknown, payload: { terminalId: string; profileId: string }) => listener(payload);
      ipcRenderer.on("ssh:terminal-session-ended", handler);
      return () => { ipcRenderer.removeListener("ssh:terminal-session-ended", handler); };
    },
    onTerminalResumed: (listener: (evt: { terminalId: string; profileId: string }) => void) => {
      const handler = (_e: unknown, payload: { terminalId: string; profileId: string }) => listener(payload);
      ipcRenderer.on("ssh:terminal-resumed", handler);
      return () => { ipcRenderer.removeListener("ssh:terminal-resumed", handler); };
    },
    // Port forwarding
    addForward: (profileId: string, localPort: number, remoteHost: string, remotePort: number, label?: string, autoStart?: boolean) =>
      ipcRenderer.invoke("ssh:add-forward", profileId, localPort, remoteHost, remotePort, label, autoStart) as Promise<{ forwardId: string }>,
    removeForward: (forwardId: string) =>
      ipcRenderer.invoke("ssh:remove-forward", forwardId),
    listForwards: (profileId?: string) =>
      ipcRenderer.invoke("ssh:list-forwards", profileId),
    openForwardBrowser: (forwardId: string) =>
      ipcRenderer.invoke("ssh:open-forward-browser", forwardId),
    onPortForwardDetected: (listener: (evt: { profileId: string; suggestion: { remotePort: number; label: string; remoteHost: string } }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; suggestion: { remotePort: number; label: string; remoteHost: string } }) => listener(payload);
      ipcRenderer.on("ssh:port-forward-detected", handler);
      return () => { ipcRenderer.removeListener("ssh:port-forward-detected", handler); };
    },
    onPortForwardStatus: (listener: (evt: { forwardId: string; status: string; actualLocalPort?: number; error?: string }) => void) => {
      const handler = (_e: unknown, payload: { forwardId: string; status: string; actualLocalPort?: number; error?: string }) => listener(payload);
      ipcRenderer.on("ssh:port-forward-status", handler);
      return () => { ipcRenderer.removeListener("ssh:port-forward-status", handler); };
    },
    // Remote file browsing
    browsePath: (profileId: string, path: string) =>
      ipcRenderer.invoke("ssh:browse-path", profileId, path) as Promise<{ entries: unknown[]; path: string; hasGitDir: boolean }>,
    homeDir: (profileId: string) =>
      ipcRenderer.invoke("ssh:home-dir", profileId) as Promise<{ path: string }>,
    openAsRepo: (profileId: string, path: string) =>
      ipcRenderer.invoke("ssh:open-as-repo", profileId, path),
    // Remote file editing
    readFile: (profileId: string, path: string) =>
      ipcRenderer.invoke("ssh:read-file", profileId, path) as Promise<{ content: string; path: string; mtime: number; size: number; language: string }>,
    writeFile: (profileId: string, path: string, content: string, expectedMtime?: number) =>
      ipcRenderer.invoke("ssh:write-file", profileId, path, content, expectedMtime) as Promise<{ success: boolean; mtime: number; conflict?: boolean }>,
    createFile: (profileId: string, path: string) =>
      ipcRenderer.invoke("ssh:create-file", profileId, path),
    deleteFile: (profileId: string, path: string) =>
      ipcRenderer.invoke("ssh:delete-file", profileId, path),
    renameFile: (profileId: string, oldPath: string, newPath: string) =>
      ipcRenderer.invoke("ssh:rename-file", profileId, oldPath, newPath),
    // Health alerts
    onHealthAlert: (listener: (evt: { profileId: string; alert: { type: string; severity: string; message: string; value: number; threshold: number } }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; alert: { type: string; severity: string; message: string; value: number; threshold: number } }) => listener(payload);
      ipcRenderer.on("ssh:health-alert", handler);
      return () => { ipcRenderer.removeListener("ssh:health-alert", handler); };
    },
    onHealthAlertsSnapshot: (listener: (evt: { profileId: string; alerts: Array<{ type: string; severity: string; message: string; value: number; threshold: number }> }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; alerts: Array<{ type: string; severity: string; message: string; value: number; threshold: number }> }) => listener(payload);
      ipcRenderer.on("ssh:health-alerts-snapshot", handler);
      return () => { ipcRenderer.removeListener("ssh:health-alerts-snapshot", handler); };
    },
    onHealthUpdate: (listener: (evt: { profileId: string; health: unknown }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; health: unknown }) => listener(payload);
      ipcRenderer.on("ssh:health-update", handler);
      return () => { ipcRenderer.removeListener("ssh:health-update", handler); };
    },
    onBandwidthUpdate: (listener: (evt: { profileId: string; bytesIn: number; bytesOut: number }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; bytesIn: number; bytesOut: number }) => listener(payload);
      ipcRenderer.on("ssh:bandwidth-update", handler);
      return () => { ipcRenderer.removeListener("ssh:bandwidth-update", handler); };
    },
    onLatencyUpdate: (listener: (evt: { profileId: string; latencyMs: number }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; latencyMs: number }) => listener(payload);
      ipcRenderer.on("ssh:latency-update", handler);
      return () => { ipcRenderer.removeListener("ssh:latency-update", handler); };
    },
    onHighLatencyMode: (listener: (evt: { profileId: string; enabled: boolean }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; enabled: boolean }) => listener(payload);
      ipcRenderer.on("ssh:high-latency-mode", handler);
      return () => { ipcRenderer.removeListener("ssh:high-latency-mode", handler); };
    },
    // SSH connection status changes
    onStatusChanged: (listener: (evt: { profileId: string; status: string; error?: string }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; status: string; error?: string }) => listener(payload);
      ipcRenderer.on("ssh:status-changed", handler);
      return () => { ipcRenderer.removeListener("ssh:status-changed", handler); };
    },
    // SSH config auto-detection (first-run banner)
    onConfigDetected: (listener: (evt: { hostCount: number }) => void) => {
      const handler = (_e: unknown, payload: { hostCount: number }) => listener(payload);
      ipcRenderer.on("ssh:config-detected", handler);
      return () => { ipcRenderer.removeListener("ssh:config-detected", handler); };
    },
    importConfig: () =>
      ipcRenderer.invoke("ssh:import-config"),

    // Enhanced SSH push events
    onTransferProgress: (listener: (payload: { id: string; bytesTransferred: number; speed: number; eta: number }) => void) => {
      const handler = (_e: unknown, payload: { id: string; bytesTransferred: number; speed: number; eta: number }) => listener(payload);
      ipcRenderer.on("ssh:transfer-progress", handler);
      return () => { ipcRenderer.removeListener("ssh:transfer-progress", handler); };
    },
    onTransferStatusChanged: (listener: (payload: { transferId: string; status: string; error?: string }) => void) => {
      const handler = (_e: unknown, payload: { transferId: string; status: string; error?: string }) => listener(payload);
      ipcRenderer.on("ssh:transfer-status-changed", handler);
      return () => { ipcRenderer.removeListener("ssh:transfer-status-changed", handler); };
    },
    getQuality: (profileId: string) =>
      ipcRenderer.invoke("ssh:quality-get", profileId) as Promise<{ level: string; latencyMs: number; bandwidthMbps: number; packetLossPercent: number; measuredAt: number } | null>,
    onQualityChanged: (listener: (payload: { profileId: string; previous: string; current: string; quality: { level: string; latencyMs: number; bandwidthMbps: number; packetLossPercent: number; measuredAt: number } }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; previous: string; current: string; quality: { level: string; latencyMs: number; bandwidthMbps: number; packetLossPercent: number; measuredAt: number } }) => listener(payload);
      ipcRenderer.on("ssh:quality-changed", handler);
      return () => { ipcRenderer.removeListener("ssh:quality-changed", handler); };
    },
    // Sync pair management (invoke wrappers)
    syncAddPair: (config: { profileId: string; localDir: string; remoteDir: string; ignorePatterns?: string[] }) =>
      ipcRenderer.invoke("ssh:sync-add-pair", config) as Promise<{ syncPairId: string }>,
    syncRemovePair: (syncPairId: string) =>
      ipcRenderer.invoke("ssh:sync-remove-pair", syncPairId),
    syncStart: (syncPairId: string) =>
      ipcRenderer.invoke("ssh:sync-start", syncPairId),
    syncPause: (syncPairId: string) =>
      ipcRenderer.invoke("ssh:sync-pause", syncPairId),
    syncListPairs: (profileId: string) =>
      ipcRenderer.invoke("ssh:sync-list-pairs", profileId),
    syncListConflicts: (syncPairId: string) =>
      ipcRenderer.invoke("ssh:sync-list-conflicts", syncPairId),
    syncResolveConflict: (conflictId: string, resolution: "local" | "remote" | "both") =>
      ipcRenderer.invoke("ssh:sync-resolve-conflict", conflictId, resolution),
    syncValidateLocalDir: (localDir: string, profileId: string) =>
      ipcRenderer.invoke("ssh:sync-validate-local-dir", { localDir, profileId }) as Promise<{ valid: boolean; error?: string }>,

    // Sync status events (on/off pairs)
    onSyncStatus: (listener: (payload: { syncPairId: string; status: string; pendingChanges: number; lastSyncAt: number }) => void) => {
      const handler = (_e: unknown, payload: { syncPairId: string; status: string; pendingChanges: number; lastSyncAt: number }) => listener(payload);
      ipcRenderer.on("ssh:sync-status", handler);
      return () => { ipcRenderer.removeListener("ssh:sync-status", handler); };
    },
    offSyncStatus: () => {
      ipcRenderer.removeAllListeners("ssh:sync-status");
    },
    onSyncFileStatus: (listener: (payload: { syncPairId: string; statuses: Record<string, string> }) => void) => {
      const handler = (_e: unknown, payload: { syncPairId: string; statuses: Record<string, string> }) => listener(payload);
      ipcRenderer.on("ssh:sync-file-status", handler);
      return () => { ipcRenderer.removeListener("ssh:sync-file-status", handler); };
    },
    offSyncFileStatus: () => {
      ipcRenderer.removeAllListeners("ssh:sync-file-status");
    },
    onSyncConflict: (listener: (payload: { id: string; syncPairId: string; filePath: string; localMtime: number; remoteMtime: number }) => void) => {
      const handler = (_e: unknown, payload: { id: string; syncPairId: string; filePath: string; localMtime: number; remoteMtime: number }) => listener(payload);
      ipcRenderer.on("ssh:sync-conflict", handler);
      return () => { ipcRenderer.removeListener("ssh:sync-conflict", handler); };
    },
    offSyncConflict: () => {
      ipcRenderer.removeAllListeners("ssh:sync-conflict");
    },
    onSyncProgress: (listener: (payload: { syncPairId: string; file: string; bytesTransferred: number; totalBytes: number; speed: number }) => void) => {
      const handler = (_e: unknown, payload: { syncPairId: string; file: string; bytesTransferred: number; totalBytes: number; speed: number }) => listener(payload);
      ipcRenderer.on("ssh:sync-progress", handler);
      return () => { ipcRenderer.removeListener("ssh:sync-progress", handler); };
    },
    offSyncProgress: () => {
      ipcRenderer.removeAllListeners("ssh:sync-progress");
    },
    onSyncError: (listener: (payload: { syncPairId: string; error: string }) => void) => {
      const handler = (_e: unknown, payload: { syncPairId: string; error: string }) => listener(payload);
      ipcRenderer.on("ssh:sync-error", handler);
      return () => { ipcRenderer.removeListener("ssh:sync-error", handler); };
    },
    offSyncError: () => {
      ipcRenderer.removeAllListeners("ssh:sync-error");
    },

    // Deploy progress events (on/off pairs)
    onDeployProgress: (listener: (payload: { stage: string; percent: number; speed?: number; eta?: number }) => void) => {
      const handler = (_e: unknown, payload: { stage: string; percent: number; speed?: number; eta?: number }) => listener(payload);
      ipcRenderer.on("ssh:deploy-progress", handler);
      return () => { ipcRenderer.removeListener("ssh:deploy-progress", handler); };
    },
    offDeployProgress: () => {
      ipcRenderer.removeAllListeners("ssh:deploy-progress");
    },

    // Agent capabilities events (on/off pairs)
    onAgentCapabilities: (listener: (payload: { profileId: string; capabilities: import("./ssh-types").AgentCapabilities }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; capabilities: import("./ssh-types").AgentCapabilities }) => listener(payload);
      ipcRenderer.on("ssh:agent-capabilities", handler);
      return () => { ipcRenderer.removeListener("ssh:agent-capabilities", handler); };
    },
    offAgentCapabilities: () => {
      ipcRenderer.removeAllListeners("ssh:agent-capabilities");
    },
    onAgentUnavailable: (listener: (payload: { profileId: string; reason: string; message: string }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; reason: string; message: string }) => listener(payload);
      ipcRenderer.on("ssh:agent-unavailable", handler);
      return () => { ipcRenderer.removeListener("ssh:agent-unavailable", handler); };
    },
    offAgentUnavailable: () => {
      ipcRenderer.removeAllListeners("ssh:agent-unavailable");
    },
    onStreamData: (listener: (payload: { streamId: string; data: string }) => void) => {
      const handler = (_e: unknown, payload: { streamId: string; data: string }) => listener(payload);
      ipcRenderer.on("ssh:stream-data", handler);
      return () => { ipcRenderer.removeListener("ssh:stream-data", handler); };
    },
    onStreamEnd: (listener: (payload: { streamId: string; exitCode?: number; error?: string }) => void) => {
      const handler = (_e: unknown, payload: { streamId: string; exitCode?: number; error?: string }) => listener(payload);
      ipcRenderer.on("ssh:stream-end", handler);
      return () => { ipcRenderer.removeListener("ssh:stream-end", handler); };
    },
    onRemoteFileChanged: (listener: (payload: { profileId: string; path: string; event: string }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; path: string; event: string }) => listener(payload);
      ipcRenderer.on("ssh:remote-file-changed", handler);
      return () => { ipcRenderer.removeListener("ssh:remote-file-changed", handler); };
    },
    onAgentUpgradeAvailable: (listener: (payload: { profileId: string; currentVersion: string; availableVersion: string }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; currentVersion: string; availableVersion: string }) => listener(payload);
      ipcRenderer.on("ssh:agent-upgrade-available", handler);
      return () => { ipcRenderer.removeListener("ssh:agent-upgrade-available", handler); };
    },
    onScheduledFired: (listener: (payload: { transferId: string; result: string; error?: string }) => void) => {
      const handler = (_e: unknown, payload: { transferId: string; result: string; error?: string }) => listener(payload);
      ipcRenderer.on("ssh:scheduled-fired", handler);
      return () => { ipcRenderer.removeListener("ssh:scheduled-fired", handler); };
    },

    // Drag-and-drop upload to remote directories
    queueDropUpload: (params: { profileId: string; remoteDir: string; files: Array<{ localPath: string; relativePath: string }> }) =>
      ipcRenderer.invoke("ssh:queue-drop-upload", params) as Promise<{ error?: string } | undefined>,

    // Transfer panel operations
    transferList: () => ipcRenderer.invoke("ssh:transfer-list"),
    transferCancel: (transferId: string) => ipcRenderer.invoke("ssh:transfer-cancel", transferId),
    transferHistory: (query: { page: number; pageSize: number; search?: string; sortBy?: string; sortDir?: string; profileId?: string }) =>
      ipcRenderer.invoke("ssh:transfer-history", query),
    transferStats: (profileId: string, period: string) =>
      ipcRenderer.invoke("ssh:transfer-stats", profileId, period),
    transferListScheduled: (profileId: string) =>
      ipcRenderer.invoke("ssh:transfer-list-scheduled", profileId),
    transferSchedule: (params: { profileId: string; direction: string; localPath: string; remotePath: string; scheduledAt: number }) =>
      ipcRenderer.invoke("ssh:transfer-schedule", params),
    transferCancelSchedule: (transferId: string) =>
      ipcRenderer.invoke("ssh:transfer-cancel-schedule", transferId),
    transferCancelBatch: (ids: string[]) =>
      ipcRenderer.invoke("ssh:transfer-cancel-batch", ids),
    transferRetryBatch: (ids: string[]) =>
      ipcRenderer.invoke("ssh:transfer-retry-batch", ids),
    transferClear: (ids: string[]) =>
      ipcRenderer.invoke("ssh:transfer-clear", ids),
    transferGetBandwidth: () =>
      ipcRenderer.invoke("ssh:transfer-get-bandwidth"),
    transferSetBandwidth: (limitKBps: number) =>
      ipcRenderer.invoke("ssh:transfer-set-bandwidth", limitKBps),
    transferSetCompression: (enabled: boolean) =>
      ipcRenderer.invoke("ssh:transfer-set-compression", enabled),
    transferSetAdaptive: (enabled: boolean) =>
      ipcRenderer.invoke("ssh:transfer-set-adaptive", enabled),
  },

  // ─── Workspace ──────────────────────────────────────────────────────
  workspace: {
    open: (rootPath: string) => ipcRenderer.invoke("workspace:open", { rootPath }),
    create: () => ipcRenderer.invoke("workspace:create"),
    close: () => ipcRenderer.invoke("workspace:close"),
    refresh: () => ipcRenderer.invoke("workspace:refresh"),
    updateConfig: (config: Record<string, unknown>) =>
      ipcRenderer.invoke("workspace:update-config", config),
    addFolder: (folderPath?: string) => ipcRenderer.invoke("workspace:add-folder", folderPath ? { folderPath } : undefined),
    removeRepo: (relativePath: string) => ipcRenderer.invoke("workspace:remove-repo", { relativePath }),
    fetchAll: () => ipcRenderer.invoke("workspace:fetch-all"),
    pullAll: () => ipcRenderer.invoke("workspace:pull-all"),
    pickDirectory: () => ipcRenderer.invoke("workspace:pick-directory"),
    onDiscovered: (listener: (payload: { rootPath: string; repos: Array<{ absolutePath: string; relativePath: string; name: string }> }) => void) => {
      const handler = (_e: unknown, payload: { rootPath: string; repos: Array<{ absolutePath: string; relativePath: string; name: string }> }) => listener(payload);
      ipcRenderer.on("workspace:discovered", handler);
      return () => { ipcRenderer.removeListener("workspace:discovered", handler); };
    },
    onStatusUpdate: (listener: (statuses: Array<{ relativePath: string; branch: string | null; dirtyCount: number; ahead: number | null; behind: number | null; error: string | null }>) => void) => {
      const handler = (_e: unknown, payload: Array<{ relativePath: string; branch: string | null; dirtyCount: number; ahead: number | null; behind: number | null; error: string | null }>) => listener(payload);
      ipcRenderer.on("workspace:status-update", handler);
      return () => { ipcRenderer.removeListener("workspace:status-update", handler); };
    },
  },

  // ─── Remote Workspace ───────────────────────────────────────────────────
  remoteWorkspace: {
    discover: (profileId: string, rootPath: string, options?: { maxDepth?: number; excludePatterns?: string[] }) =>
      ipcRenderer.invoke("remote-workspace:discover", { profileId, rootPath, options }),
    open: (profileId: string, rootPath: string, options?: { maxDepth?: number; excludePatterns?: string[] }) =>
      ipcRenderer.invoke("remote-workspace:open", { profileId, rootPath, options }),
    close: (profileId: string) =>
      ipcRenderer.invoke("remote-workspace:close", { profileId }),
    refresh: (profileId: string, rootPath: string, options?: { maxDepth?: number; excludePatterns?: string[] }) =>
      ipcRenderer.invoke("remote-workspace:refresh", { profileId, rootPath, options }),
    onDiscovered: (listener: (payload: { profileId: string; rootPath: string; repos: Array<{ absolute_path: string; relative_path: string; name: string }> }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; rootPath: string; repos: Array<{ absolute_path: string; relative_path: string; name: string }> }) => listener(payload);
      ipcRenderer.on("remote-workspace:discovered", handler);
      return () => { ipcRenderer.removeListener("remote-workspace:discovered", handler); };
    },
    onStatusUpdate: (listener: (payload: { profileId: string; statuses: Array<{ relative_path: string; branch: string | null; dirty_count: number; ahead: number | null; behind: number | null; error: string | null }>; poll_timestamp: number }) => void) => {
      const handler = (_e: unknown, payload: { profileId: string; statuses: Array<{ relative_path: string; branch: string | null; dirty_count: number; ahead: number | null; behind: number | null; error: string | null }>; poll_timestamp: number }) => listener(payload);
      ipcRenderer.on("remote-workspace:status-update", handler);
      return () => { ipcRenderer.removeListener("remote-workspace:status-update", handler); };
    },
    // Remote workspace source persistence
    addSource: (params: { workspaceRoot: string; profileId: string; profileLabel: string; rootPath: string; maxDepth?: number; excludePatterns?: string[] }) =>
      ipcRenderer.invoke("ssh:remote-source-add", params),
    removeSource: (sourceId: string) =>
      ipcRenderer.invoke("ssh:remote-source-remove", sourceId),
    listSources: (workspaceRoot: string) =>
      ipcRenderer.invoke("ssh:remote-source-list", workspaceRoot),
    listAllSources: () =>
      ipcRenderer.invoke("ssh:remote-source-list-all"),
  },

  // ─── VFS Mounts ────────────────────────────────────────────────────────
  mount: {
    list: () => ipcRenderer.invoke("mount:list"),
    mount: (params: { repo: string; mountpoint: string; ref?: string; readOnly?: boolean }) =>
      ipcRenderer.invoke("mount:mount", params),
    unmount: (mountpoint: string) =>
      ipcRenderer.invoke("mount:unmount", { mountpoint }),
    status: (mountpoint: string) =>
      ipcRenderer.invoke("mount:status", { mountpoint }),
    refresh: (mountpoint: string) =>
      ipcRenderer.invoke("mount:refresh", { mountpoint }),
    switch: (mountpoint: string, ref: string) =>
      ipcRenderer.invoke("mount:switch", { mountpoint, ref }),
  },

  // ─── LSP ────────────────────────────────────────────────────────────────
  lsp: {
    start: (language: string, rootPath: string) =>
      ipcRenderer.invoke("lsp:start", language, rootPath),
    send: (language: string, message: string) =>
      ipcRenderer.invoke("lsp:send", language, message),
    stop: (language: string) =>
      ipcRenderer.invoke("lsp:stop", language),
    discover: () =>
      ipcRenderer.invoke("lsp:discover"),
    status: () =>
      ipcRenderer.invoke("lsp:status"),
    onMessage: (listener: (payload: { language: string; message: string }) => void) => {
      const handler = (_e: unknown, payload: { language: string; message: string }) => listener(payload);
      ipcRenderer.on("lsp:message", handler);
      return () => { ipcRenderer.removeListener("lsp:message", handler); };
    },
    onStatusChange: (listener: (payload: { language: string; state: string; command?: string; error?: string }) => void) => {
      const handler = (_e: unknown, payload: { language: string; state: string; command?: string; error?: string }) => listener(payload);
      ipcRenderer.on("lsp:status", handler);
      return () => { ipcRenderer.removeListener("lsp:status", handler); };
    },
    install: (language: string) =>
      ipcRenderer.invoke("lsp:install", language),
    installSupported: (language: string) =>
      ipcRenderer.invoke("lsp:install-supported", language) as Promise<boolean>,
    onInstallProgress: (listener: (payload: { language: string; phase: string; percent?: number; downloaded?: number; total?: number; message?: string; installedPath?: string }) => void) => {
      const handler = (_e: unknown, payload: { language: string; phase: string; percent?: number; downloaded?: number; total?: number; message?: string; installedPath?: string }) => listener(payload);
      ipcRenderer.on("lsp:install-progress", handler);
      return () => { ipcRenderer.removeListener("lsp:install-progress", handler); };
    },
  },

  // ─── Orchestrator ──────────────────────────────────────────────────────
  orchestrator: {
    createTask: (payload: { prompt: string; agentType: string; repoPath?: string; repoId?: string; baseRef?: string; planFirst?: boolean; contextFiles?: string[]; model?: string; intelligence?: string; title?: string; worktreeMode?: "auto" | "primary" | "existing"; worktreePath?: string; skills?: string[] }) =>
      ipcRenderer.invoke("orchestrator:create-task", {
        prompt: payload.prompt,
        agentType: payload.agentType,
        repoPath: payload.repoPath ?? payload.repoId,
        baseRef: payload.baseRef,
        planFirst: payload.planFirst,
        contextFiles: payload.contextFiles,
        model: payload.model,
        intelligence: payload.intelligence,
        title: payload.title,
        worktreeMode: payload.worktreeMode,
        worktreePath: payload.worktreePath,
        skills: payload.skills,
      }),
    cancelTask: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:cancel-task", { taskId }),
    iterate: (taskId: string, feedback: string) =>
      ipcRenderer.invoke("orchestrator:iterate", { taskId, feedback }),
    applyAll: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:apply-all", { taskId }),
    cherryPick: (taskId: string, files: string[]) =>
      ipcRenderer.invoke("orchestrator:cherry-pick", { taskId, files }),
    discard: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:discard", { taskId }),
    listTasks: (payload?: { repoPath?: string }) =>
      ipcRenderer.invoke("orchestrator:list-tasks", payload),
    getDiff: (taskId: string, checkpointSha?: string, checkpointFromSha?: string) =>
      ipcRenderer.invoke("orchestrator:get-diff", { taskId, checkpointSha, checkpointFromSha }),
    reviewWithAi: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:review-with-ai", { taskId }),
    revertFile: (taskId: string, filePath: string) =>
      ipcRenderer.invoke("orchestrator:revert-file", { taskId, filePath }),
    getCommits: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:get-commits", { taskId }),
    detectAgents: (options?: { force?: boolean; cachedOnly?: boolean }) =>
      ipcRenderer.invoke("orchestrator:detect-agents", options),
    testAgent: (agentType: string) =>
      ipcRenderer.invoke("orchestrator:test-agent", { agentType }),
    listRegisteredAgents: () =>
      ipcRenderer.invoke("orchestrator:list-registered-agents"),
    getAgentConfig: (agentType: string) =>
      ipcRenderer.invoke("orchestrator:get-agent-config", { agentType }),
    setAgentConfig: (agentType: string, config: unknown) =>
      ipcRenderer.invoke("orchestrator:set-agent-config", { agentType, config }),
    reloadManifests: () =>
      ipcRenderer.invoke("orchestrator:reload-manifests"),
    approvePlan: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:approve-plan", { taskId }),
    rejectPlan: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:reject-plan", { taskId }),
    respondApproval: (
      taskId: string,
      payload: {
        requestId: string;
        decision: "allow" | "allow_once" | "deny";
        summary: string;
        note?: string;
      },
    ) =>
      ipcRenderer.invoke("orchestrator:respond-approval", { taskId, ...payload }),
    fetchIssues: (payload: { provider: string }) =>
      ipcRenderer.invoke("orchestrator:fetch-issues", payload),
    pickContextFiles: () =>
      ipcRenderer.invoke("orchestrator:pick-context-files"),
    // Workspace scripts
    runSetupScript: (payload: { taskId: string; repoPath: string; setupScript?: string; runScript?: string; archiveScript?: string; portRangeStart?: number; portRangeEnd?: number }) =>
      ipcRenderer.invoke("orchestrator:run-setup-script", payload),
    runWorkspace: (payload: { taskId: string; repoPath: string; command?: string; setupScript?: string; runScript?: string; archiveScript?: string; portRangeStart?: number; portRangeEnd?: number }) =>
      ipcRenderer.invoke("orchestrator:run-workspace", payload),
    stopWorkspace: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:stop-workspace", { taskId }),
    spotlightSync: (taskId: string, repoPath: string) =>
      ipcRenderer.invoke("orchestrator:spotlight-sync", { taskId, repoPath }),
    spotlightRevert: (taskId: string, repoPath: string) =>
      ipcRenderer.invoke("orchestrator:spotlight-revert", { taskId, repoPath }),
    // Hunk-level apply / revert
    applyHunk: (taskId: string, patch: { filePath: string; oldString: string; newString: string }) =>
      ipcRenderer.invoke("orchestrator:apply-hunk", { taskId, patch }),
    revertHunk: (taskId: string, patch: { filePath: string; oldString: string; newString: string }) =>
      ipcRenderer.invoke("orchestrator:revert-hunk", { taskId, patch }),
    // Per-turn cost breakdown
    getTurnCosts: (workspaceId: string) =>
      ipcRenderer.invoke("orchestrator:get-turn-costs", { workspaceId }),
    // Most recent checkpoint (for resume prompts)
    latestCheckpoint: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:latest-checkpoint", { taskId }),
    // Working-tree diff vs HEAD for "Attach diff"
    getWorktreeDiff: (payload: { taskId?: string; cwd?: string }) =>
      ipcRenderer.invoke("orchestrator:get-worktree-diff", payload),
    // Todos
    addTodo: (taskId: string, text: string, source?: "user" | "agent" | "comment") =>
      ipcRenderer.invoke("orchestrator:add-todo", { taskId, text, source }),
    toggleTodo: (taskId: string, todoId: string) =>
      ipcRenderer.invoke("orchestrator:toggle-todo", { taskId, todoId }),
    removeTodo: (taskId: string, todoId: string) =>
      ipcRenderer.invoke("orchestrator:remove-todo", { taskId, todoId }),
    // PR Creation & Management
    createPr: (taskId: string, title?: string, body?: string, draft?: boolean) =>
      ipcRenderer.invoke("orchestrator:create-pr", { taskId, title, body, draft }),
    updatePr: (taskId: string, title?: string, body?: string) =>
      ipcRenderer.invoke("orchestrator:update-pr", { taskId, title, body }),
    mergePr: (taskId: string, prNumber: number, method?: "merge" | "squash" | "rebase") =>
      ipcRenderer.invoke("orchestrator:merge-pr", { taskId, prNumber, method }),
    // Checks
    getChecks: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:get-checks", { taskId }),
    // Archiving
    archiveTask: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:archive-task", { taskId }),
    restoreTask: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:restore-task", { taskId }),
    deletePermanently: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:delete-permanently", { taskId }),
    // Retry & Undo
    retryTask: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:retry-task", { taskId }),
    undoApply: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:undo-apply", { taskId }),
    // Multi-Agent
    addAgent: (taskId: string, agentType: string, role?: string) =>
      ipcRenderer.invoke("orchestrator:add-agent", { taskId, agentType, role }),
    setTaskPinned: (taskId: string, pinned: boolean) =>
      ipcRenderer.invoke("orchestrator:set-task-pinned", { taskId, pinned }),
    pinTask: (taskId: string, pinned: boolean) =>
      ipcRenderer.invoke("orchestrator:set-task-pinned", { taskId, pinned }),
    renameTask: (taskId: string, title: string) =>
      ipcRenderer.invoke("orchestrator:rename-task", { taskId, title }),
    setTaskParent: (taskId: string, parentTaskId?: string | null) =>
      ipcRenderer.invoke("orchestrator:set-task-parent", { taskId, parentTaskId }),
    setTaskSidebarGroup: (taskId: string, sidebarGroup?: string | null) =>
      ipcRenderer.invoke("orchestrator:set-task-sidebar-group", { taskId, sidebarGroup }),
    // Preferences
    getPreferences: (repoPath: string) =>
      ipcRenderer.invoke("orchestrator:get-preferences", { repoPath }),
    savePreferences: (repoPath: string, preferences: { general: string; codeReview: string; pr: string; conflictResolution: string }) =>
      ipcRenderer.invoke("orchestrator:save-preferences", { repoPath, preferences }),
    // Context & MCP
    addContextFile: (taskId: string, fileName: string, content: string) =>
      ipcRenderer.invoke("orchestrator:add-context-file", { taskId, fileName, content }),
    listContextFiles: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:list-context-files", { taskId }),
    listWorktreeFiles: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:list-worktree-files", { taskId }),
    removeContextFile: (taskId: string, fileName: string) =>
      ipcRenderer.invoke("orchestrator:remove-context-file", { taskId, fileName }),
    getMcpServers: (repoPath: string) =>
      ipcRenderer.invoke("orchestrator:get-mcp-servers", { repoPath }),
    // Cost
    getCost: (workspaceId: string) =>
      ipcRenderer.invoke("orchestrator:get-cost", { workspaceId }),
    getAllCosts: () =>
      ipcRenderer.invoke("orchestrator:get-all-costs"),
    exportCostsCsv: () =>
      ipcRenderer.invoke("orchestrator:export-costs-csv"),
    // Checkpoints
    revertCheckpoint: (taskId: string, turnNumber: number) =>
      ipcRenderer.invoke("orchestrator:revert-checkpoint", { taskId, turnNumber }),
    listCheckpoints: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:list-checkpoints", { taskId }),
    onCheckpointCreated: (listener: (payload: { taskId: string; turnNumber: number; ref: string; sha: string }) => void) => {
      const handler = (_e: unknown, payload: { taskId: string; turnNumber: number; ref: string; sha: string }) => listener(payload);
      ipcRenderer.on("orchestrator:checkpoint-created", handler);
      return () => { ipcRenderer.removeListener("orchestrator:checkpoint-created", handler); };
    },
    // Branch rename
    renameBranch: (taskId: string, newBranch: string) =>
      ipcRenderer.invoke("orchestrator:rename-branch", { taskId, newBranch }),
    // Slash commands
    listCommands: (repoPath: string) =>
      ipcRenderer.invoke("orchestrator:list-commands", { repoPath }),
    expandCommand: (name: string, args: Record<string, string>) =>
      ipcRenderer.invoke("orchestrator:expand-command", { name, args }),
    createCommand: (name: string, scope: "repo" | "user", content: string) =>
      ipcRenderer.invoke("orchestrator:create-command", { name, scope, content }),
    deleteCommand: (name: string) =>
      ipcRenderer.invoke("orchestrator:delete-command", { name }),
    readCommandSource: (name: string) =>
      ipcRenderer.invoke("orchestrator:read-command-source", { name }),
    scaffoldCommand: (payload: { name: string; description: string; argumentHint?: string; category?: string }) =>
      ipcRenderer.invoke("orchestrator:scaffold-command", payload),
    // Chat persistence
    saveChat: (taskId: string, messages: unknown[]) =>
      ipcRenderer.invoke("orchestrator:save-chat", { taskId, messages }),
    loadChat: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:load-chat", { taskId }),
    searchChat: (query: string, taskId?: string, limit?: number) =>
      ipcRenderer.invoke("orchestrator:search-chat", { query, taskId, limit }),
    // Chat UI state persistence
    setTaskUIState: (taskId: string, state: unknown) =>
      ipcRenderer.invoke("orchestrator:set-task-ui-state", { taskId, state }),
    getTaskUIState: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:get-task-ui-state", { taskId }),

    // ─── Thread Operations (Codex app-server) ──────────────────────────
    forkThread: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:fork-thread", { taskId }),
    rollbackThread: (taskId: string, n: number) =>
      ipcRenderer.invoke("orchestrator:rollback-thread", { taskId, n }),
    compactThread: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:compact-thread", { taskId }),
    startReview: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:start-review", { taskId }),
    setThreadName: (taskId: string, name: string) =>
      ipcRenderer.invoke("orchestrator:set-thread-name", { taskId, name }),
    setGoal: (taskId: string, goal: string) =>
      ipcRenderer.invoke("orchestrator:set-goal", { taskId, goal }),
    getGoal: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:get-goal", { taskId }),
    clearGoal: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:clear-goal", { taskId }),
    updateThreadMetadata: (taskId: string, metadata: Record<string, unknown>) =>
      ipcRenderer.invoke("orchestrator:update-thread-metadata", { taskId, metadata }),
    shellCommand: (taskId: string, command: string, cwd?: string) =>
      ipcRenderer.invoke("orchestrator:shell-command", { taskId, command, cwd }),
    cleanBackgroundTerminals: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:clean-background-terminals", { taskId }),
    injectItems: (taskId: string, items: Array<{ role: string; content: unknown }>) =>
      ipcRenderer.invoke("orchestrator:inject-items", { taskId, items }),

    // ─── Command Execution (Codex app-server) ──────────────────────────
    execCommand: (taskId: string, params: { command: string; args?: string[]; cwd?: string; env?: Record<string, string>; stdin?: string; timeoutMs?: number }) =>
      ipcRenderer.invoke("orchestrator:exec-command", { taskId, ...params }),
    writeCommandStdin: (taskId: string, sessionId: string, stdin: string, closeStdin?: boolean) =>
      ipcRenderer.invoke("orchestrator:write-command-stdin", { taskId, sessionId, stdin, closeStdin }),
    resizeCommandPty: (taskId: string, sessionId: string, cols: number, rows: number) =>
      ipcRenderer.invoke("orchestrator:resize-command-pty", { taskId, sessionId, cols, rows }),
    terminateCommand: (taskId: string, sessionId: string) =>
      ipcRenderer.invoke("orchestrator:terminate-command", { taskId, sessionId }),

    // ─── Feedback (Codex app-server) ──────────────────────────────────
    uploadFeedback: (taskId: string, params: { turnId?: string; rating?: number; comment?: string; tags?: string[] }) =>
      ipcRenderer.invoke("orchestrator:upload-feedback", { taskId, ...params }),

    // ─── Filesystem (Codex app-server) ─────────────────────────────────
    fsReadFile: (path: string, taskId?: string) =>
      ipcRenderer.invoke("orchestrator:fs-read-file", { path, taskId }),
    fsWriteFile: (path: string, content: string, taskId?: string) =>
      ipcRenderer.invoke("orchestrator:fs-write-file", { path, content, taskId }),
    fsCreateDirectory: (path: string, taskId?: string) =>
      ipcRenderer.invoke("orchestrator:fs-create-directory", { path, taskId }),
    fsGetMetadata: (path: string, taskId?: string) =>
      ipcRenderer.invoke("orchestrator:fs-get-metadata", { path, taskId }),
    fsReadDirectory: (path: string, taskId?: string) =>
      ipcRenderer.invoke("orchestrator:fs-read-directory", { path, taskId }),
    fsRemove: (path: string, recursive?: boolean, taskId?: string) =>
      ipcRenderer.invoke("orchestrator:fs-remove", { path, recursive, taskId }),
    fsCopy: (source: string, destination: string, taskId?: string) =>
      ipcRenderer.invoke("orchestrator:fs-copy", { source, destination, taskId }),
    fsWatch: (path: string, taskId?: string) =>
      ipcRenderer.invoke("orchestrator:fs-watch", { path, taskId }),
    fsUnwatch: (watchId: string, taskId?: string) =>
      ipcRenderer.invoke("orchestrator:fs-unwatch", { watchId, taskId }),

    // ─── Event Subscriptions ────────────────────────────────────────────
    onCommandOutput: (listener: (payload: { taskId: string; sessionId: string; delta: string }) => void) => {
      const handler = (_e: unknown, payload: { taskId: string; sessionId: string; delta: string }) => listener(payload);
      ipcRenderer.on("orchestrator:command-output", handler);
      return () => { ipcRenderer.removeListener("orchestrator:command-output", handler); };
    },
    onCommandExited: (listener: (payload: { taskId: string; sessionId: string; exitCode: number }) => void) => {
      const handler = (_e: unknown, payload: { taskId: string; sessionId: string; exitCode: number }) => listener(payload);
      ipcRenderer.on("orchestrator:command-exited", handler);
      return () => { ipcRenderer.removeListener("orchestrator:command-exited", handler); };
    },
    onFsChanged: (listener: (payload: { taskId: string; watchId: string; path: string; kind: string }) => void) => {
      const handler = (_e: unknown, payload: { taskId: string; watchId: string; path: string; kind: string }) => listener(payload);
      ipcRenderer.on("orchestrator:fs-changed", handler);
      return () => { ipcRenderer.removeListener("orchestrator:fs-changed", handler); };
    },

    onEvent: (listener: (payload: { taskId: string; event: unknown }) => void) => {
      const handler = (_e: unknown, payload: { taskId: string; event: unknown }) => listener(payload);
      ipcRenderer.on("orchestrator:event", handler);
      return () => { ipcRenderer.removeListener("orchestrator:event", handler); };
    },
    onTaskStatus: (listener: (payload: { taskId: string; status: string }) => void) => {
      const handler = (_e: unknown, payload: { taskId: string; status: string }) => listener(payload);
      ipcRenderer.on("orchestrator:task-status", handler);
      return () => { ipcRenderer.removeListener("orchestrator:task-status", handler); };
    },
    onDiffReady: (listener: (payload: { taskId: string; diff: string; files: unknown[] }) => void) => {
      const handler = (_e: unknown, payload: { taskId: string; diff: string; files: unknown[] }) => listener(payload);
      ipcRenderer.on("orchestrator:diff-ready", handler);
      return () => { ipcRenderer.removeListener("orchestrator:diff-ready", handler); };
    },
    onError: (listener: (payload: { taskId: string | null; message: string }) => void) => {
      const handler = (_e: unknown, payload: { taskId: string | null; message: string }) => listener(payload);
      ipcRenderer.on("orchestrator:error", handler);
      return () => { ipcRenderer.removeListener("orchestrator:error", handler); };
    },
    onNonconcurrentStop: (listener: (payload: { stoppedTaskId: string; startedTaskId: string }) => void) => {
      const handler = (_e: unknown, payload: { stoppedTaskId: string; startedTaskId: string }) => listener(payload);
      ipcRenderer.on("orchestrator:nonconcurrent-stop", handler);
      return () => { ipcRenderer.removeListener("orchestrator:nonconcurrent-stop", handler); };
    },
    onBudgetWarning: (listener: (payload: { taskId: string; cost: number; limit: number; workspaceTitle?: string }) => void) => {
      const handler = (_e: unknown, payload: { taskId: string; cost: number; limit: number; workspaceTitle?: string }) => listener(payload);
      ipcRenderer.on("orchestrator:budget-warning", handler);
      return () => { ipcRenderer.removeListener("orchestrator:budget-warning", handler); };
    },
    resumeBudget: (taskId: string) =>
      ipcRenderer.invoke("orchestrator:resume-budget", { taskId }),
  },

  // ─── Agent Workflows ───────────────────────────────────────────────────
  agentWorkflows: {
    listDefinitions: (repoPath: string) =>
      ipcRenderer.invoke("agent-workflow:list-definitions", { repoPath }),
    getDefinition: (repoPath: string, definitionId: string) =>
      ipcRenderer.invoke("agent-workflow:get-definition", { repoPath, definitionId }),
    saveDefinition: (repoPath: string, definition: unknown) =>
      ipcRenderer.invoke("agent-workflow:save-definition", { repoPath, definition }),
    deleteDefinition: (repoPath: string, definitionId: string) =>
      ipcRenderer.invoke("agent-workflow:delete-definition", { repoPath, definitionId }),
    listVersions: (definitionId: string) =>
      ipcRenderer.invoke("agent-workflow:list-versions", { definitionId }),
    getVersion: (definitionId: string, version: number) =>
      ipcRenderer.invoke("agent-workflow:get-version", { definitionId, version }),
    revertToVersion: (repoPath: string, definitionId: string, version: number) =>
      ipcRenderer.invoke("agent-workflow:revert-to-version", { repoPath, definitionId, version }),
    simulateRun: (definition: unknown, input?: unknown) =>
      ipcRenderer.invoke("agent-workflow:simulate-run", { definition, input }),
    validateDefinition: (definition: unknown) =>
      ipcRenderer.invoke("agent-workflow:validate-definition", { definition }),
    startRun: (repoPath: string, definitionId: string, options?: unknown) =>
      ipcRenderer.invoke("agent-workflow:start-run", { repoPath, definitionId, options }),
    getRun: (runId: string) =>
      ipcRenderer.invoke("agent-workflow:get-run", { runId }),
    listRuns: (repoPath?: string) =>
      ipcRenderer.invoke("agent-workflow:list-runs", { repoPath }),
    listEvents: (runId: string) =>
      ipcRenderer.invoke("agent-workflow:list-events", { runId }),
    cancelRun: (runId: string) =>
      ipcRenderer.invoke("agent-workflow:cancel-run", { runId }),
    pauseRun: (runId: string) =>
      ipcRenderer.invoke("agent-workflow:pause-run", { runId }),
    resumeRun: (runId: string) =>
      ipcRenderer.invoke("agent-workflow:resume-run", { runId }),
    approveNode: (runId: string, nodeRunId: string, summary?: string) =>
      ipcRenderer.invoke("agent-workflow:approve-node", { runId, nodeRunId, summary }),
    requestNodeChanges: (runId: string, nodeRunId: string, feedback: string) =>
      ipcRenderer.invoke("agent-workflow:request-node-changes", { runId, nodeRunId, feedback }),
    requestHumanReviewChanges: (runId: string, nodeRunId: string, feedback: string) =>
      ipcRenderer.invoke("agent-workflow:request-human-review-changes", { runId, nodeRunId, feedback }),
    rejectNode: (runId: string, nodeRunId: string, reason: string) =>
      ipcRenderer.invoke("agent-workflow:reject-node", { runId, nodeRunId, reason }),
    retryNode: (runId: string, nodeRunId: string, options?: unknown) =>
      ipcRenderer.invoke("agent-workflow:retry-node", { runId, nodeRunId, options }),
    listTriggers: (repoPath?: string) =>
      ipcRenderer.invoke("agent-workflow:list-triggers", { repoPath }),
    saveTrigger: (trigger: unknown) =>
      ipcRenderer.invoke("agent-workflow:save-trigger", { trigger }),
    deleteTrigger: (triggerId: string) =>
      ipcRenderer.invoke("agent-workflow:delete-trigger", { triggerId }),
    setTriggerEnabled: (triggerId: string, enabled: boolean) =>
      ipcRenderer.invoke("agent-workflow:set-trigger-enabled", { triggerId, enabled }),
    onRunUpdated: (listener: (payload: unknown) => void) => {
      const handler = (_e: unknown, payload: unknown) => listener(payload);
      ipcRenderer.on("agent-workflow:run-updated", handler);
      return () => { ipcRenderer.removeListener("agent-workflow:run-updated", handler); };
    },
    onNodeEvent: (listener: (payload: unknown) => void) => {
      const handler = (_e: unknown, payload: unknown) => listener(payload);
      ipcRenderer.on("agent-workflow:node-event", handler);
      return () => { ipcRenderer.removeListener("agent-workflow:node-event", handler); };
    },
    onTriggerEvent: (listener: (payload: unknown) => void) => {
      const handler = (_e: unknown, payload: unknown) => listener(payload);
      ipcRenderer.on("agent-workflow:trigger-event", handler);
      return () => { ipcRenderer.removeListener("agent-workflow:trigger-event", handler); };
    },

  },

  // ─── File path resolution ──────────────────────────────────────────────
  //
  // Electron 32+ removed `File.path` on renderer-side File objects. The
  // supported replacement is `webUtils.getPathForFile()` — which must be
  // called from preload, since `webUtils` isn't available in the sandboxed
  // renderer. Drop targets and hidden <input type="file"> elements use
  // this to turn native File objects into absolute paths the agent can
  // read from disk.
  //
  // See https://www.electronjs.org/docs/latest/api/web-utils
  getPathForFile: (file: File): string => {
    try {
      return webUtils.getPathForFile(file);
    } catch {
      return "";
    }
  },

  // ─── Auto-Updater ──────────────────────────────────────────────────────
  updater: {
    checkForUpdate: () =>
      ipcRenderer.invoke("updater:check") as Promise<{ success: boolean; version?: string | null; error?: string }>,
    downloadUpdate: () =>
      ipcRenderer.invoke("updater:download") as Promise<{ success: boolean; error?: string }>,
    quitAndInstall: () =>
      ipcRenderer.invoke("updater:install"),
    getVersion: () =>
      ipcRenderer.invoke("updater:get-version") as Promise<string>,
    onChecking: (listener: () => void) => {
      const handler = () => listener();
      ipcRenderer.on("updater:checking", handler);
      return () => { ipcRenderer.removeListener("updater:checking", handler); };
    },
    onUpdateAvailable: (listener: (info: { version: string; releaseDate: string; releaseNotes: unknown }) => void) => {
      const handler = (_e: unknown, payload: { version: string; releaseDate: string; releaseNotes: unknown }) => listener(payload);
      ipcRenderer.on("updater:available", handler);
      return () => { ipcRenderer.removeListener("updater:available", handler); };
    },
    onUpdateNotAvailable: (listener: (info: { version: string }) => void) => {
      const handler = (_e: unknown, payload: { version: string }) => listener(payload);
      ipcRenderer.on("updater:not-available", handler);
      return () => { ipcRenderer.removeListener("updater:not-available", handler); };
    },
    onUnavailable: (listener: (info: { message: string }) => void) => {
      const handler = (_e: unknown, payload: { message: string }) => listener(payload);
      ipcRenderer.on("updater:unavailable", handler);
      return () => { ipcRenderer.removeListener("updater:unavailable", handler); };
    },
    onProgress: (listener: (progress: { percent: number; bytesPerSecond: number; transferred: number; total: number }) => void) => {
      const handler = (_e: unknown, payload: { percent: number; bytesPerSecond: number; transferred: number; total: number }) => listener(payload);
      ipcRenderer.on("updater:progress", handler);
      return () => { ipcRenderer.removeListener("updater:progress", handler); };
    },
    onUpdateDownloaded: (listener: (info: { version: string; releaseDate: string; releaseNotes: unknown }) => void) => {
      const handler = (_e: unknown, payload: { version: string; releaseDate: string; releaseNotes: unknown }) => listener(payload);
      ipcRenderer.on("updater:downloaded", handler);
      return () => { ipcRenderer.removeListener("updater:downloaded", handler); };
    },
    onError: (listener: (err: { message: string }) => void) => {
      const handler = (_e: unknown, payload: { message: string }) => listener(payload);
      ipcRenderer.on("updater:error", handler);
      return () => { ipcRenderer.removeListener("updater:error", handler); };
    },
  },

  // ─── Browser ──────────────────────────────────────────────────────
  browser: {
    registerGuest: (args: { browserTabId: string; worktreeId: string; webContentsId: number }) =>
      ipcRenderer.invoke("browser:register-guest", args) as Promise<boolean>,
    unregisterGuest: (args: { browserTabId: string }) =>
      ipcRenderer.invoke("browser:unregister-guest", args) as Promise<boolean>,
    openDevtools: (args: { browserTabId: string }) =>
      ipcRenderer.invoke("browser:open-devtools", args) as Promise<boolean>,
    acceptDownload: (args: { downloadId: string }) =>
      ipcRenderer.invoke("browser:accept-download", args) as Promise<{ ok: boolean; reason?: string }>,
    cancelDownload: (args: { downloadId: string }) =>
      ipcRenderer.invoke("browser:cancel-download", args) as Promise<boolean>,
    setViewportOverride: (args: { browserTabId: string; override: { width: number; height: number; deviceScaleFactor: number; mobile: boolean } | null }) =>
      ipcRenderer.invoke("browser:set-viewport-override", args) as Promise<boolean>,
    executeJavaScript: (args: { browserTabId: string; code: string }) =>
      ipcRenderer.invoke("browser:execute-javascript", args) as Promise<{ ok: boolean; result?: unknown; error?: string }>,
    getTabState: (args: { browserTabId: string }) =>
      ipcRenderer.invoke("browser:get-tab-state", args) as Promise<unknown>,
    openUrlExternal: (args: { url: string }) =>
      ipcRenderer.invoke("browser:open-url-external", args) as Promise<void>,
    session: {
      listProfiles: () =>
        ipcRenderer.invoke("browser:session:listProfiles") as Promise<Array<{ id: string; scope: string; partition: string; label: string; source: Record<string, unknown> }>>,
      createProfile: (args: { scope: string; label: string }) =>
        ipcRenderer.invoke("browser:session:createProfile", args) as Promise<{ id: string; scope: string; partition: string; label: string; source: Record<string, unknown> } | null>,
      deleteProfile: (args: { profileId: string }) =>
        ipcRenderer.invoke("browser:session:deleteProfile", args) as Promise<boolean>,
      resolvePartition: (args: { profileId: string | null }) =>
        ipcRenderer.invoke("browser:session:resolvePartition", args) as Promise<string | null>,
      importCookies: (args: { profileId: string }) =>
        ipcRenderer.invoke("browser:session:importCookies", args) as Promise<{ ok: boolean; reason?: string; summary?: { totalCookies: number; imported: number; skipped: number } }>,
      clearDefaultCookies: () =>
        ipcRenderer.invoke("browser:session:clearDefaultCookies") as Promise<boolean>,
      detectBrowsers: () =>
        ipcRenderer.invoke("browser:session:detectBrowsers") as Promise<Array<{ family: string | null; label: string; profiles: { name: string; directory: string }[]; selectedProfile: string }>>,
      importFromBrowser: (args: { profileId: string; browserFamily: string; browserProfile?: string }) =>
        ipcRenderer.invoke("browser:session:importFromBrowser", args) as Promise<{ ok: boolean; reason?: string; summary?: { totalCookies: number; imported: number; skipped: number } }>,
    },
    // ── Grab & Annotation ───────────────────────────────────────────
    setGrabMode: (args: { browserPageId: string; enabled: boolean }) =>
      ipcRenderer.invoke("browser:setGrabMode", args) as Promise<{ ok: true } | { ok: false; reason: string }>,
    awaitGrabSelection: (args: { browserPageId: string; opId: string }) =>
      ipcRenderer.invoke("browser:awaitGrabSelection", args) as Promise<unknown>,
    cancelGrab: (args: { browserPageId: string }) =>
      ipcRenderer.invoke("browser:cancelGrab", args) as Promise<boolean>,
    captureSelectionScreenshot: (args: { browserPageId: string; rect: { x: number; y: number; width: number; height: number } }) =>
      ipcRenderer.invoke("browser:captureSelectionScreenshot", args) as Promise<{ ok: boolean; screenshot?: { mimeType: string; dataUrl: string; width: number; height: number }; reason?: string }>,
    extractHoverPayload: (args: { browserPageId: string }) =>
      ipcRenderer.invoke("browser:extractHoverPayload", args) as Promise<{ ok: boolean; payload?: unknown; reason?: string }>,
    screenshotToClipboard: (args: { browserPageId: string }) =>
      ipcRenderer.invoke("browser:screenshotToClipboard", args) as Promise<boolean>,
    setAnnotationViewportBridge: (args: { browserPageId: string; enabled: boolean; emitViewport: boolean; markers: Array<{ id: string; index: number; rectPage: { x: number; y: number; width: number; height: number }; rectViewport: { x: number; y: number; width: number; height: number }; isFixed: boolean }>; token: string }) =>
      ipcRenderer.invoke("browser:setAnnotationViewportBridge", args) as Promise<boolean>,
    getAnnotations: (args: { browserPageId: string }) =>
      ipcRenderer.invoke("browser:getAnnotations", args) as Promise<unknown[]>,
    annotateSelection: (args: { browserPageId: string; id: string; comment: string; intent: string; priority: string; payload: unknown }) =>
      ipcRenderer.invoke("browser:annotateSelection", args) as Promise<boolean>,
    deleteAnnotation: (args: { browserPageId: string; annotationId: string }) =>
      ipcRenderer.invoke("browser:deleteAnnotation", args) as Promise<boolean>,
  },
  onBrowserNavigated: (listener: (payload: { browserTabId: string; url: string }) => void) => {
    const handler = (_e: unknown, payload: { browserTabId: string; url: string }) => listener(payload);
    ipcRenderer.on("browser:navigated", handler);
    return () => { ipcRenderer.removeListener("browser:navigated", handler); };
  },
  onBrowserPageTitleUpdated: (listener: (payload: { browserTabId: string; title: string }) => void) => {
    const handler = (_e: unknown, payload: { browserTabId: string; title: string }) => listener(payload);
    ipcRenderer.on("browser:page-title-updated", handler);
    return () => { ipcRenderer.removeListener("browser:page-title-updated", handler); };
  },
  onBrowserDidStartLoading: (listener: (payload: { browserTabId: string }) => void) => {
    const handler = (_e: unknown, payload: { browserTabId: string }) => listener(payload);
    ipcRenderer.on("browser:did-start-loading", handler);
    return () => { ipcRenderer.removeListener("browser:did-start-loading", handler); };
  },
  onBrowserDidStopLoading: (listener: (payload: { browserTabId: string }) => void) => {
    const handler = (_e: unknown, payload: { browserTabId: string }) => listener(payload);
    ipcRenderer.on("browser:did-stop-loading", handler);
    return () => { ipcRenderer.removeListener("browser:did-stop-loading", handler); };
  },
  onBrowserDownloadRequested: (listener: (payload: { downloadId: string; browserTabId: string; origin: string; filename: string; totalBytes: number | null; mimeType: string | null }) => void) => {
    const handler = (_e: unknown, payload: { downloadId: string; browserTabId: string; origin: string; filename: string; totalBytes: number | null; mimeType: string | null }) => listener(payload);
    ipcRenderer.on("browser:download-requested", handler);
    return () => { ipcRenderer.removeListener("browser:download-requested", handler); };
  },
  onBrowserDownloadProgress: (listener: (payload: { downloadId: string; browserTabId: string; receivedBytes: number; totalBytes: number }) => void) => {
    const handler = (_e: unknown, payload: { downloadId: string; browserTabId: string; receivedBytes: number; totalBytes: number }) => listener(payload);
    ipcRenderer.on("browser:download-progress", handler);
    return () => { ipcRenderer.removeListener("browser:download-progress", handler); };
  },
  onBrowserDownloadFinished: (listener: (payload: { downloadId: string; browserTabId: string; status: string; savePath: string | null; error: string | null }) => void) => {
    const handler = (_e: unknown, payload: { downloadId: string; browserTabId: string; status: string; savePath: string | null; error: string | null }) => listener(payload);
    ipcRenderer.on("browser:download-finished", handler);
    return () => { ipcRenderer.removeListener("browser:download-finished", handler); };
  },
  // Browser permission denied events
  onBrowserPermissionDenied: (listener: (payload: { permission: string }) => void) => {
    const handler = (_e: unknown, payload: { permission: string }) => listener(payload);
    ipcRenderer.on("browser:permission-denied", handler);
    return () => { ipcRenderer.removeListener("browser:permission-denied", handler); };
  },
  // Browser grab mode toggle events (main → renderer)
  onGrabModeToggle: (listener: (browserPageId: string) => void) => {
    const handler = (_e: unknown, browserPageId: string) => listener(browserPageId);
    ipcRenderer.on("browser:grabModeToggle", handler);
    return () => { ipcRenderer.removeListener("browser:grabModeToggle", handler); };
  },
  // Browser grab action shortcut events (main → renderer)
  onGrabActionShortcut: (listener: (payload: { browserPageId: string; key: string }) => void) => {
    const handler = (_e: unknown, payload: { browserPageId: string; key: string }) => listener(payload);
    ipcRenderer.on("browser:grabActionShortcut", handler);
    return () => { ipcRenderer.removeListener("browser:grabActionShortcut", handler); };
  },
  // Browser context menu request event (main → renderer)
  onBrowserContextMenuRequested: (listener: (payload: { browserPageId: string; x: number; y: number; screenX: number; screenY: number; pageUrl: string; linkUrl: string | null; canGoBack: boolean; canGoForward: boolean }) => void) => {
    const handler = (_e: unknown, payload: unknown) => listener(payload as { browserPageId: string; x: number; y: number; screenX: number; screenY: number; pageUrl: string; linkUrl: string | null; canGoBack: boolean; canGoForward: boolean });
    ipcRenderer.on("browser:context-menu-requested", handler);
    return () => { ipcRenderer.removeListener("browser:context-menu-requested", handler); };
  },
  onBrowserContextMenu: (listener: (payload: { browserTabId: string; x: number; y: number; pageUrl: string; linkUrl: string | null; canGoBack: boolean; canGoForward: boolean }) => void) => {
    const handler = (_e: unknown, payload: { browserTabId: string; x: number; y: number; pageUrl: string; linkUrl: string | null; canGoBack: boolean; canGoForward: boolean }) => listener(payload);
    ipcRenderer.on("browser:context-menu", handler);
    return () => { ipcRenderer.removeListener("browser:context-menu", handler); };
  },
  onBrowserDomReady: (listener: (payload: { browserTabId: string; canGoBack: boolean; canGoForward: boolean }) => void) => {
    const handler = (_e: unknown, payload: { browserTabId: string; canGoBack: boolean; canGoForward: boolean }) => listener(payload);
    ipcRenderer.on("browser:dom-ready", handler);
    return () => { ipcRenderer.removeListener("browser:dom-ready", handler); };
  },
  onBrowserNavigationState: (listener: (payload: { browserTabId: string; canGoBack: boolean; canGoForward: boolean }) => void) => {
    const handler = (_e: unknown, payload: { browserTabId: string; canGoBack: boolean; canGoForward: boolean }) => listener(payload);
    ipcRenderer.on("browser:navigation-state", handler);
    return () => { ipcRenderer.removeListener("browser:navigation-state", handler); };
  },
  onBrowserPageFaviconUpdated: (listener: (payload: { browserTabId: string; faviconUrl: string | null }) => void) => {
    const handler = (_e: unknown, payload: { browserTabId: string; faviconUrl: string | null }) => listener(payload);
    ipcRenderer.on("browser:page-favicon-updated", handler);
    return () => { ipcRenderer.removeListener("browser:page-favicon-updated", handler); };
  },
  onBrowserCertificateError: (listener: (payload: { browserTabId: string; url: string; error: string; certificate: any }) => void) => {
    const handler = (_e: unknown, payload: { browserTabId: string; url: string; error: string; certificate: any }) => listener(payload);
    ipcRenderer.on("browser:certificate-error", handler);
    return () => { ipcRenderer.removeListener("browser:certificate-error", handler); };
  },

  // ─── Skill Manager ──────────────────────────────────────────────────
  skills: {
    listSkills: (opts?: { scope?: string; repoPath?: string }) =>
      ipcRenderer.invoke("skills:list", opts),
    getSkill: (id: string) =>
      ipcRenderer.invoke("skills:get", id),
    createSkill: (data: { name: string; description: string; body?: string; scope: string; repoPath?: string }) =>
      ipcRenderer.invoke("skills:create", data),
    updateSkill: (data: { id: string; updates: { name?: string; description?: string; body?: string } }) =>
      ipcRenderer.invoke("skills:update", data),
    deleteSkill: (id: string) =>
      ipcRenderer.invoke("skills:delete", id),
    installFromUrl: (url: string, scope: string, repoPath?: string) =>
      ipcRenderer.invoke("skills:install-from-url", url, scope, repoPath),
    toggleEnabled: (id: string, enabled: boolean) =>
      ipcRenderer.invoke("skills:toggle-enabled", id, enabled),
    setActivationPrefs: (id: string, prefs: unknown) =>
      ipcRenderer.invoke("skills:set-activation-prefs", id, prefs),
    detectAgents: () =>
      ipcRenderer.invoke("skills:detect-agents"),
    resyncAll: () =>
      ipcRenderer.invoke("skills:resync-all"),
  },
});
