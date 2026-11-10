/**
 * IPC Bridge for the Crab Desktop Orchestrator.
 *
 * Registers IPC handlers for all orchestrator commands (renderer → main)
 * and relays orchestrator events back to the renderer (main → renderer).
 */

import type { BrowserWindow, IpcMain } from "electron";
import { dialog } from "electron";

import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { randomUUID } from "node:crypto";

import { Orchestrator, type Task } from "./orchestrator";
import type { PersistedChatUIState } from "./orchestrator";
import { ChatDatabase } from "./orchestrator-chat-db";
import { registerChatIpc } from "./orchestrator-chat";
import { registerDiffIpc } from "./orchestrator-diff";
import { detectAgents, getCachedAgents, prewarmAgents, type AgentEvent, type AgentInputResponse } from "./orchestrator-adapters";
import { fetchGitHubIssues, fetchLinearIssues } from "./orchestrator-issues";
import { CostTracker, TokenUsage } from "./orchestrator-cost";
import { agentRegistry } from "./orchestrator-registry";
import { loadAgentConfig, saveAgentConfig } from "./orchestrator-settings";
import type { WorkspaceScriptEvent, WorkspaceScriptOptions } from "./orchestrator-scripts";
import { TemplateManager, WorkspaceTemplate } from "./orchestrator-templates";
import { SlashCommandManager } from "./orchestrator-commands";
import { WorkspaceSearch } from "./orchestrator-search";
import type { SearchFilters } from "./orchestrator-search";
import { CIAutoFixer } from "./orchestrator-autofix";
import type { HunkPatch } from "./orchestrator-hunks";
import { registerModelDiscoveryIpc } from "./orchestrator-models";
import type { SkillManager } from "./skill-manager";

/**
 * Registers all orchestrator IPC handlers on the given `ipcMain` instance.
 *
 * Creates an `Orchestrator` instance, wires renderer → main command handlers,
 * and relays orchestrator events to the renderer via `webContents.send()`.
 *
 * Returns the orchestrator and shutdown of its native conversation services.
 */
export function registerOrchestratorIpc(
  ipcMain: IpcMain,
  getMainWindow: () => BrowserWindow | null,
  skillManager?: SkillManager,
): { orchestrator: Orchestrator; shutdown: () => Promise<void> } {
  const orchestrator = new Orchestrator();

  const chatDb = new ChatDatabase();
  const chat = registerChatIpc(ipcMain, orchestrator, chatDb, getMainWindow);

  const startCreatedTask = (task: Task) => {
    // Creation must return the task ID before native startup asks for input.
    // Following up on an existing chat can await the full startup result.
    void orchestrator.startTask(task.id).catch((error: unknown) => {
      if (["stopped", "discarded", "archived", "applied"].includes(task.status)) return;
      orchestrator.emit("orchestrator:error", { taskId: task.id, message: error instanceof Error ? error.message : String(error) });
    });
  };

  orchestrator.on("orchestrator:task-status", (payload: { taskId: string; status: import("./orchestrator").TaskStatus }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed() && !win.webContents.isDestroyed()) {
      win.webContents.send("orchestrator:task-status", payload);
    }
  });

  orchestrator.on("orchestrator:error", (payload: { taskId: string | null; message: string }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed() && !win.webContents.isDestroyed()) {
      win.webContents.send("orchestrator:error", payload);
    }
  });

  orchestrator.on(
    "orchestrator:iteration-warning",
    (payload: { taskId: string; iterations: number; message: string }) => {
      const win = getMainWindow();
      if (win && !win.isDestroyed() && !win.webContents.isDestroyed()) {
        win.webContents.send("orchestrator:iteration-warning", payload);
      }
    },
  );

  orchestrator.on(
    "orchestrator:checkpoint-created",
    (payload: { taskId: string; turnNumber: number; ref: string; sha: string }) => {
      const win = getMainWindow();
      if (win && !win.isDestroyed() && !win.webContents.isDestroyed()) {
        win.webContents.send("orchestrator:checkpoint-created", payload);
      }
    },
  );

  orchestrator.on("orchestrator:files-changed", (payload: { taskIds: string[] }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed() && !win.webContents.isDestroyed()) win.webContents.send("orchestrator:files-changed", payload);
  });

  orchestrator.on(
    "orchestrator:nonconcurrent-stop",
    (payload: { stoppedTaskId: string; startedTaskId: string }) => {
      const win = getMainWindow();
      if (win && !win.isDestroyed() && !win.webContents.isDestroyed()) {
        win.webContents.send("orchestrator:nonconcurrent-stop", payload);
      }
    },
  );

  // ─── Template Manager ─────────────────────────────────────────────────────

  const templateManager = new TemplateManager();

  // ─── Slash Command Manager ──────────────────────────────────────────────

  const commandManager = new SlashCommandManager();

  // ─── Cost Tracker ───────────────────────────────────────────────────────

  const costTracker = new CostTracker();
  costTracker.load();

  // When a usage event arrives from an adapter, record it in the cost tracker.
  orchestrator.on("orchestrator:event", (payload: { taskId: string; event: AgentEvent }) => {
    if (payload.event.type !== "usage" || !payload.event.usage) return;

    const task = orchestrator.getTask(payload.taskId);
    if (!task) return;

    const usage: TokenUsage = {
      inputTokens: payload.event.usage.inputTokens,
      outputTokens: payload.event.usage.outputTokens,
      cacheReadTokens: payload.event.usage.cacheReadInputTokens,
      cacheWriteTokens: payload.event.usage.cacheCreationInputTokens,
      timestamp: Date.now(),
      turnId: payload.event.turnId ?? `${payload.taskId}:${task.pendingInput?.turnNumber ?? task.turnNumber ?? 0}`,
      sessionId: payload.event.usage.cumulative?.sessionId,
      estimatedCostUsd: payload.event.usage.cumulative?.costUsd,
    };

    costTracker.recordUsage(payload.taskId, usage, task.agentType, payload.event.usage.model ?? task.model ?? "");
    costTracker.persist();

    // Relay budget warnings to the renderer.
    const budgetCheck = costTracker.checkBudget(payload.taskId);
    if (budgetCheck.exceeded) {
      const win = getMainWindow();
      if (win && !win.isDestroyed() && !win.webContents.isDestroyed()) {
        win.webContents.send("orchestrator:budget-warning", {
          workspaceId: payload.taskId,
          cost: budgetCheck.cost,
          limit: budgetCheck.limit,
        });
      }
    }
  });

  // ─── Command Handlers (Renderer → Main) ─────────────────────────────────

  /**
   * Resolves an array of skill IDs to SKILL.md content strings.
   * Returns the concatenated skill context and any resolution errors.
   */
  async function resolveSkillContext(skillIds: string[]): Promise<{
    context: string;
    errors: string[];
  }> {
    const errors: string[] = [];
    const contents: string[] = [];

    if (!skillManager) {
      return { context: "", errors: ["Skill manager not available"] };
    }

    for (const id of skillIds) {
      const result = await skillManager.getSkill(id);
      if (result.success && result.data) {
        contents.push(
          `### ${result.data.frontmatter.name || result.data.record.name}\n\n${result.data.body}`,
        );
      } else {
        errors.push(`Skill "${id}": ${result.error ?? "not found"}`);
      }
    }

    if (contents.length === 0) return { context: "", errors };

    const context = [
      "## Available Skills",
      "",
      "The following skills are available for this task. Use the instructions",
      "below when they are relevant to the work.",
      "",
      ...contents,
      "",
      "---",
      "",
    ].join("\n");

    return { context, errors };
  }

  ipcMain.handle(
    "orchestrator:create-task",
    async (_evt, payload: { prompt: string; agentType: string; repoPath: string; baseRef?: string; planFirst?: boolean; model?: string; intelligence?: string; title?: string; worktreeMode?: "auto" | "primary" | "existing" | "new"; worktreePath?: string; skills?: string[] }) => {
      try {
        console.log("[orchestrator] create-task:", { repoPath: payload.repoPath, agentType: payload.agentType, baseRef: payload.baseRef, model: payload.model, intelligence: payload.intelligence, worktreeMode: payload.worktreeMode });

        // Map display names to agent type IDs.
        // The renderer may send either the display name ("Claude Code") or
        // the type ID ("claude-code"). Normalize to the type ID.
        const agentTypeMap: Record<string, string> = {
          "Claude Code": "claude-code",
          "claude-code": "claude-code",
          "Codex": "codex",
          "codex": "codex",
          "Custom": "custom",
          "custom": "custom",
        };
        const normalizedAgentType = agentTypeMap[payload.agentType] ?? payload.agentType;
        // If the name contains "claude" (case-insensitive), treat as claude-code.
        const resolvedAgentType: "claude-code" | "codex" | "custom" =
          (normalizedAgentType === "claude-code" || normalizedAgentType === "codex" || normalizedAgentType === "custom")
            ? normalizedAgentType
            : payload.agentType.toLowerCase().includes("claude")
              ? "claude-code"
              : payload.agentType.toLowerCase().includes("codex")
                ? "codex"
                : "custom";

        let effectivePrompt = payload.prompt;

        // Inject skill content into the prompt when skills are provided.
        if (payload.skills && payload.skills.length > 0) {
          const { context, errors } = await resolveSkillContext(payload.skills);
          if (context) {
            effectivePrompt = context + effectivePrompt;
          }
          if (errors.length > 0) {
            console.warn("[orchestrator] skill resolution warnings:", errors);
          }
        }

        // Use HEAD as baseRef if not specified or if the specified ref doesn't exist
        const baseRef = payload.baseRef || "HEAD";

        const task = await orchestrator.createTask(
          effectivePrompt,
          resolvedAgentType,
          payload.repoPath,
          baseRef,
          {
            worktreeMode:
              payload.worktreeMode === "primary" || payload.worktreeMode === "existing"
                ? payload.worktreeMode
                : "auto",
            worktreePath: payload.worktreePath,
          },
        );
        console.log("[orchestrator] task created:", task.id, "worktree:", task.worktreePath, "agent:", resolvedAgentType);

        // Apply display and steering metadata before starting the agent, so
        // reloads retain the user's title and the CLI sees the right flags.
        const task2 = orchestrator.getTask(task.id);
        if (task2) {
          task2.title = payload.title?.trim() || payload.prompt.trim().slice(0, 80) || task2.title;
          if (payload.model) (task2 as any).model = payload.model;
          if (payload.intelligence) (task2 as any).reasoningLevel = payload.intelligence;
          if (payload.planFirst) task2.planFirst = true;
          await orchestrator.persistTasks();
        }

        // Start the task in the background — don't await so the renderer
        // gets the real task ID immediately. This eliminates the dead zone
        // where events arrive under the real ID but the renderer is still
        // showing the optimistic ID. The task-status event will notify the
        // renderer when the agent is actually running.
        startCreatedTask(task);

        return { success: true, task };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        console.error("[orchestrator] create-task FAILED:", message);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:cancel-task",
    async (_evt, payload: { taskId: string }) => {
      try {
        await orchestrator.cancelTask(payload.taskId);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // Decisions answer the original live request; they do not start another turn.
  ipcMain.handle(
    "orchestrator:respond-approval",
    async (_evt, payload: { taskId: string; requestId: string; decision: string; summary: string; note?: string }) => {
      try {
        const ok = await orchestrator.recordApproval(payload.taskId, {
          requestId: payload.requestId,
          decision: payload.decision,
          summary: payload.summary,
          note: payload.note,
        });
        if (!ok) {
          return { success: false, error: "This approval is no longer pending" };
        }
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle("orchestrator:respond-input", (_evt, payload: { taskId: string; requestId: string; response: AgentInputResponse }) => {
    try {
      return orchestrator.recordInput(payload.taskId, payload.requestId, payload.response)
        ? { success: true }
        : { success: false, error: "This input request is no longer pending" };
    } catch (err: unknown) {
      return { success: false, error: err instanceof Error ? err.message : "Could not send the answer" };
    }
  });

  ipcMain.handle(
    "orchestrator:reject-plan",
    async (_evt, payload: { taskId: string }) => {
      try {
        await orchestrator.iterate(
          payload.taskId,
          "Plan rejected. Please propose a different approach.",
        );
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle("orchestrator:apply-all", async (_event, payload: { taskId: string }) => {
    try { return await orchestrator.applyTask(payload.taskId); }
    catch (error: unknown) { return { success: false, error: error instanceof Error ? error.message : String(error) }; }
  });
  ipcMain.handle("orchestrator:cherry-pick", async (_event, payload: { taskId: string; files: string[] }) => {
    try { return await orchestrator.applyTask(payload.taskId, payload.files); }
    catch (error: unknown) { return { success: false, error: error instanceof Error ? error.message : String(error) }; }
  });

  ipcMain.handle(
    "orchestrator:discard",
    async (_evt, payload: { taskId: string; archiveScript?: string; repoPath?: string; portRangeStart?: number; portRangeEnd?: number }) => {
      try {
        await orchestrator.discardTask(payload.taskId, payload);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:list-tasks",
    async (_evt, payload?: { repoPath?: string }) => {
      try {
        let tasks = orchestrator.getAllTasks();
        // Filter by repo if a repoPath is provided.
        if (payload?.repoPath) {
          tasks = tasks.filter((t) => t.repoPath === payload.repoPath);
        }
        return { success: true, tasks };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  registerDiffIpc(ipcMain, orchestrator, getMainWindow);


  ipcMain.handle(
    "orchestrator:get-commits",
    async (_evt, payload: { taskId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        const { execFile: execFileCb } = await import("node:child_process");
        const { promisify } = await import("node:util");
        const execFile = promisify(execFileCb);
        const { stdout } = await execFile(
          "git",
          ["log", "--oneline", `${task.baseRef}..${task.branch}`],
          { cwd: task.repoPath },
        );
        const commits = stdout
          .trim()
          .split("\n")
          .filter((l: string) => l.trim())
          .map((line: string) => {
            const spaceIdx = line.indexOf(" ");
            return {
              sha: line.slice(0, spaceIdx),
              message: line.slice(spaceIdx + 1),
            };
          });
        return { success: true, commits };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:detect-agents",
    async (_event, payload?: { force?: boolean; cachedOnly?: boolean }) => {
      try {
        // `cachedOnly` lets the UI grab whatever's already known without
        // waiting for a probe — used for the initial render path where
        // responsiveness trumps freshness.
        if (payload?.cachedOnly) {
          const cached = getCachedAgents();
          return {
            success: true,
            agents: cached ?? [],
            cached: cached !== null,
          };
        }
        const agents = await detectAgents({ force: payload?.force });
        return { success: true, agents };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  const stopModelDiscovery = registerModelDiscoveryIpc(ipcMain, orchestrator);

  // Fire off a probe in the background so the first UI request hits a
  // warm cache. Any errors surface later through the regular handler.
  prewarmAgents();

  ipcMain.handle("orchestrator:test-agent", async (_event, { agentType }: { agentType: string }) => {
    try {
      // Use registry to check if agent type is registered
      if (!agentRegistry.has(agentType)) {
        return { success: false, error: `Agent type "${agentType}" is not registered` };
      }
      const agents = await detectAgents();
      const registration = agentRegistry.listRegistered().find(r => r.type === agentType);
      const binary = (registration?.manifest as any)?.binary ??
        (agentType === "claude-code" ? "claude" : agentType === "codex" ? "codex" : agentType);
      const target = agents.find(a => a.binary === binary);
      if (!target || !target.available) {
        return { success: false, error: `Agent "${agentType}" not found on PATH` };
      }
      return { success: true, agent: target };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  // ─── Agent Registry (Renderer → Main) ─────────────────────────────────

  ipcMain.handle("orchestrator:list-registered-agents", async () => {
    try {
      const registrations = agentRegistry.listRegistered();
      const agents = registrations.map(reg => ({
        type: reg.type,
        name: reg.name,
        capabilities: reg.capabilities,
        builtin: reg.builtin,
        manifest: reg.manifest ? {
          binary: (reg.manifest as any).binary,
          versionFlag: (reg.manifest as any).versionFlag,
          requiredEnv: (reg.manifest as any).requiredEnv,
          interactionModel: (reg.manifest as any).interactionModel,
        } : undefined,
      }));
      return { success: true, agents };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:get-agent-config", async (_event, { agentType }: { agentType: string }) => {
    try {
      const config = loadAgentConfig(agentType);
      return { success: true, config };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:set-agent-config", async (_event, { agentType, config }: { agentType: string; config: unknown }) => {
    try {
      saveAgentConfig(agentType, config);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:reload-manifests", async () => {
    try {
      agentRegistry.loadManifests(path.join(os.homedir(), ".crab", "agents"));
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  // ─── Issue Fetching (Renderer → Main) ─────────────────────────────────

  ipcMain.handle(
    "orchestrator:fetch-issues",
    async (_evt, payload: { provider: string }) => {
      try {
        if (payload.provider === "linear") {
          return await fetchLinearIssues();
        }
        return await fetchGitHubIssues();
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Context File Picker (Renderer → Main) ────────────────────────────
  //
  // Backs the "Attach → File / Image / Document" menu in the chat composer
  // and the "Add context file" button in the New Task dialog. Both call
  // this without arguments and expect either a string[] of absolute paths
  // or null when the user cancels.
  //
  // The sniff at the filter level matters because images and docs are
  // frequently attached alongside prompts (screenshots of a bug, PDFs of
  // a spec, etc.) and showing them in the dialog by default is a lot
  // friendlier than forcing users through "All files (*)".

  ipcMain.handle("orchestrator:pick-context-files", async () => {
    const win = getMainWindow();
    if (!win) return null;
    const res = await dialog.showOpenDialog(win, {
      title: "Attach files",
      properties: ["openFile", "multiSelections"],
      filters: [
        { name: "All supported", extensions: [
          // Text / code
          "md", "txt", "rtf",
          // Documents
          "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
          // Images
          "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp",
        ] },
        { name: "Images", extensions: ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"] },
        { name: "Documents", extensions: ["pdf", "doc", "docx", "md", "txt", "rtf", "xlsx", "pptx"] },
        { name: "All files", extensions: ["*"] },
      ],
    });
    if (res.canceled || res.filePaths.length === 0) return null;
    return res.filePaths;
  });

  orchestrator.on("orchestrator:workspace-script", (event: WorkspaceScriptEvent) => {
    const win = getMainWindow();
    if (!win || win.isDestroyed() || win.webContents.isDestroyed()) return;
    if (event.type === "nonconcurrent-stop") {
      win.webContents.send("orchestrator:nonconcurrent-stop", { stoppedTaskId: event.stoppedTaskId, startedTaskId: event.startedTaskId });
    } else if (event.type === "exit") {
      win.webContents.send("orchestrator:terminal-exit", { taskId: event.taskId, exitCode: event.exitCode });
    } else if (event.kind === "run") {
      win.webContents.send("orchestrator:terminal-data", { taskId: event.taskId, data: event.data });
    } else {
      win.webContents.send("orchestrator:event", { taskId: event.taskId,
        event: { type: "command_run", timestamp: Date.now(), stdout: event.data, command: `${event.kind}-script` } });
    }
  });

  ipcMain.handle("orchestrator:run-setup-script", async (_event, payload: WorkspaceScriptOptions & { taskId: string }) => {
    try { return await orchestrator.runSetupScript(payload.taskId, payload); }
    catch (error: unknown) { return { success: false, error: error instanceof Error ? error.message : String(error) }; }
  });
  ipcMain.handle("orchestrator:run-workspace", async (_event, payload: WorkspaceScriptOptions & { taskId: string }) => {
    try { return { success: true, ...await orchestrator.runWorkspace(payload.taskId, payload) }; }
    catch (error: unknown) { return { success: false, error: error instanceof Error ? error.message : String(error) }; }
  });
  ipcMain.handle("orchestrator:stop-workspace", async (_event, payload: { taskId: string }) => {
    try { await orchestrator.stopWorkspace(payload.taskId); return { success: true }; }
    catch (error: unknown) { return { success: false, error: error instanceof Error ? error.message : String(error) }; }
  });

  ipcMain.handle(
    "orchestrator:spotlight-sync",
    async (_evt, payload: { taskId: string; repoPath: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const { execFile: execFileCb } = await import("node:child_process");
        const { promisify } = await import("node:util");
        const execFileAsync = promisify(execFileCb);

        // Generate a diff from the worktree and apply it to the repo root.
        const { stdout: diff } = await execFileAsync(
          "git",
          ["diff", task.baseRef, "HEAD"],
          { cwd: task.worktreePath, maxBuffer: 10 * 1024 * 1024 },
        );

        if (!diff.trim()) {
          return { success: true, message: "No changes to sync" };
        }

        // Apply the diff to the repo root.
        const { execFile: execFileCb2 } = await import("node:child_process");
        const execFile2 = promisify(execFileCb2);
        await execFile2("git", ["apply", "--3way", "-"], {
          cwd: payload.repoPath,
          maxBuffer: 10 * 1024 * 1024,
          // @ts-ignore - input option exists
          input: diff,
        });

        return { success: true, message: "Changes synced to repo root" };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:spotlight-revert",
    async (_evt, payload: { taskId: string; repoPath: string }) => {
      try {
        const { execFile: execFileCb } = await import("node:child_process");
        const { promisify } = await import("node:util");
        const execFileAsync = promisify(execFileCb);

        // Revert any uncommitted changes in the repo root.
        await execFileAsync("git", ["checkout", "--", "."], { cwd: payload.repoPath });
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Hunk-level apply / revert ────────────────────────────────────────
  //
  // Backs the approve/revert buttons surfaced on inline Edit tool cards
  // in the chat. Paths are resolved against the task's worktree so the
  // same payload (relative or absolute) works no matter how the tool
  // call reported it. We deliberately restrict writes to the task's
  // worktree to avoid editing unrelated files via this channel.

  ipcMain.handle(
    "orchestrator:apply-hunk",
    async (_evt, payload: { taskId: string; patch: HunkPatch }) => {
      try {
        await orchestrator.editHunk(payload.taskId, payload.patch, "apply");
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:revert-hunk",
    async (_evt, payload: { taskId: string; patch: HunkPatch }) => {
      try {
        await orchestrator.editHunk(payload.taskId, payload.patch, "revert");
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Chat UI State (Renderer → Main) ─────────────────────────────────

  ipcMain.handle(
    "orchestrator:set-task-ui-state",
    async (_evt, payload: { taskId: string; state: PersistedChatUIState | null }) => {
      try {
        await orchestrator.setTaskUIState(payload.taskId, payload.state);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:set-task-pinned",
    async (_evt, payload: { taskId: string; pinned: boolean }) => {
      try {
        const task = await orchestrator.setTaskPinned(payload.taskId, payload.pinned);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        return { success: true, task };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:set-task-sidebar-group",
    async (_evt, payload: { taskId: string; sidebarGroup?: string | null }) => {
      try {
        const task = await orchestrator.setTaskSidebarGroup(payload.taskId, payload.sidebarGroup ?? null);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        return { success: true, task };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:rename-task",
    async (_evt, payload: { taskId: string; title: string }) => {
      try {
        const task = await orchestrator.renameTask(payload.taskId, payload.title);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        return { success: true, task };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:set-task-parent",
    async (_evt, payload: { taskId: string; parentTaskId?: string | null }) => {
      try {
        const task = await orchestrator.setTaskParent(payload.taskId, payload.parentTaskId ?? undefined);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        return { success: true, task };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:get-task-ui-state",
    async (_evt, payload: { taskId: string }) => {
      try {
        const state = orchestrator.getTaskUIState(payload.taskId) ?? null;
        return { success: true, state };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Per-turn cost inspection ─────────────────────────────────────────

  ipcMain.handle(
    "orchestrator:get-turn-costs",
    async (_evt, payload: { workspaceId: string }) => {
      try {
        const turns = costTracker.getPerTurnCosts(payload.workspaceId);
        return { success: true, turns };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Latest checkpoint (for "Resume where you left off") ──────────────
  //
  // Exposes the most recent checkpoint ref + SHA for each task. The
  // renderer uses this on app start to surface a resume prompt if the
  // previous session ended mid-turn.

  ipcMain.handle(
    "orchestrator:latest-checkpoint",
    async (_evt, payload: { taskId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        const checkpoints = await orchestrator.checkpointManager(task).listCheckpoints(task);
        if (checkpoints.length === 0) {
          return { success: true, latest: null };
        }
        const latest = checkpoints[checkpoints.length - 1];
        return { success: true, latest };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Current git diff vs HEAD (for "Attach diff" composer chip) ──────
  //
  // Returns the unified diff of the worktree vs HEAD so the composer
  // can attach "what's changed so far" with one click. Bounded to keep
  // the prompt sane.

  ipcMain.handle(
    "orchestrator:get-worktree-diff",
    async (_evt, payload: { taskId?: string; cwd?: string }) => {
      try {
        const { execFile: execFileCb } = await import("node:child_process");
        const { promisify } = await import("node:util");
        const execFileAsync = promisify(execFileCb);

        let cwd: string | undefined;
        if (payload.taskId) {
          const task = orchestrator.getTask(payload.taskId);
          if (!task) return { success: false, error: `Task not found: ${payload.taskId}` };
          cwd = task.worktreePath ?? task.repoPath;
        } else if (payload.cwd) {
          cwd = payload.cwd;
        }
        if (!cwd || !fs.existsSync(cwd)) {
          return { success: false, error: "No valid working directory for diff" };
        }

        // Limit to 200KB — anything larger will be a very noisy prompt.
        const maxBuffer = 200 * 1024;
        const [head, stat] = await Promise.all([
          execFileAsync("git", ["diff", "HEAD", "--"], { cwd, maxBuffer }).catch(() => ({ stdout: "" })),
          execFileAsync("git", ["diff", "HEAD", "--stat"], { cwd, maxBuffer: 64 * 1024 }).catch(() => ({ stdout: "" })),
        ]);
        return {
          success: true,
          diff: head.stdout,
          stat: stat.stdout,
          truncated: head.stdout.length >= maxBuffer - 1024,
        };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Checkpoints (Renderer → Main) ────────────────────────────────────

  ipcMain.handle(
    "orchestrator:restore-files",
    async (_evt, payload: { taskId: string; sourceRef: string }) => {
      try {
        await orchestrator.restoreFiles(payload.taskId, payload.sourceRef);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:list-checkpoints",
    async (_evt, payload: { taskId: string }) => {
      try {
        await orchestrator.recoverTaskInput(payload.taskId);
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        const checkpoints = await orchestrator.checkpointManager(task).listCheckpoints(task);
        const backups = await orchestrator.checkpointManager(task).listRestoreBackups(task);
        return { success: true, checkpoints, backups };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Todos (Renderer → Main) ──────────────────────────────────────────

  ipcMain.handle(
    "orchestrator:add-todo",
    async (_evt, payload: { taskId: string; text: string; source?: "user" | "agent" | "comment" }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const todo = {
          id: `todo-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
          text: payload.text,
          done: false,
          source: payload.source ?? "user" as const,
        };

        // Initialize todos array if not present.
        if (!(task as any).todos) {
          (task as any).todos = [];
        }
        (task as any).todos.push(todo);
        await orchestrator.persistTasks();

        return { success: true, todo };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:toggle-todo",
    async (_evt, payload: { taskId: string; todoId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const todos: any[] = (task as any).todos ?? [];
        const todo = todos.find((t) => t.id === payload.todoId);
        if (!todo) {
          return { success: false, error: `Todo not found: ${payload.todoId}` };
        }

        todo.done = !todo.done;
        await orchestrator.persistTasks();

        return { success: true, todo };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:remove-todo",
    async (_evt, payload: { taskId: string; todoId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const todos: any[] = (task as any).todos ?? [];
        const idx = todos.findIndex((t) => t.id === payload.todoId);
        if (idx === -1) {
          return { success: false, error: `Todo not found: ${payload.todoId}` };
        }

        todos.splice(idx, 1);
        await orchestrator.persistTasks();

        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── PR Creation (Renderer → Main) ────────────────────────────────────

  ipcMain.handle(
    "orchestrator:create-pr",
    async (_evt, payload: { taskId: string; title?: string; body?: string; draft?: boolean }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const { createPR, loadPRTemplate, buildPRDescriptionPrompt } = await import("./orchestrator-pr.js");
        const { computeDiff, computeRemoteDiff } = await import("./orchestrator-diff.js");

        // Load PR template if available
        const template = loadPRTemplate(task.repoPath);

        // Generate AI-drafted description if no title/body provided
        let title = payload.title;
        let body = payload.body;

        if (!title || !body) {
          let diff;
          if (task.isRemote && task.remoteProfileId) {
            const router = orchestrator.getRpcRouter();
            if (router) {
              diff = await computeRemoteDiff(task, router);
            } else {
              diff = await computeDiff(task);
            }
          } else {
            diff = await computeDiff(task);
          }
          // Build prompt for AI-drafted description (stored for renderer to use with AI SDK)
          void buildPRDescriptionPrompt(diff.rawDiff, task.prompt, template);

          // Return the prompt for the renderer to call AI with
          // For now, use a simple title from the task prompt
          if (!title) {
            title = task.prompt.slice(0, 70).replace(/\n/g, " ");
          }
          if (!body) {
            body = template
              ? template.replace(/<!-- .* -->/g, "").trim() + `\n\n## Changes\n\n${task.prompt}`
              : `## Summary\n\n${task.prompt}\n\n## Changes\n\nSee diff for details.`;
          }
        }

        const result = await createPR({
          repoPath: task.repoPath,
          branch: task.branch,
          baseBranch: task.baseRef,
          title,
          body,
          draft: payload.draft,
        });

        if (result.success && result.pr) {
          // Store PR metadata in task
          (task as any).prUrl = result.pr.url;
          (task as any).prNumber = result.pr.number;
          await orchestrator.persistTasks();
        }

        return result;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:update-pr",
    async (_evt, payload: { taskId: string; title?: string; body?: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const prNumber = (task as any).prNumber;
        if (!prNumber) {
          return { success: false, error: "No PR associated with this task" };
        }

        const { updatePR } = await import("./orchestrator-pr.js");
        return await updatePR({
          repoPath: task.repoPath,
          branch: task.branch,
          prNumber,
          title: payload.title,
          body: payload.body,
        });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Checks Tab (Renderer → Main) ─────────────────────────────────────

  ipcMain.handle(
    "orchestrator:get-checks",
    async (_evt, payload: { taskId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const { getGitStatus, getPRChecks } = await import("./orchestrator-pr.js");

        // Get git status
        const gitStatus = await getGitStatus(task.worktreePath, task.baseRef);

        // Get PR checks if PR exists
        const prNumber = (task as any).prNumber;
        let pr = undefined;
        let checks: any[] = [];
        let comments: any[] = [];

        if (prNumber) {
          const prChecks = await getPRChecks(task.repoPath, prNumber);
          if (prChecks.success && prChecks.data) {
            pr = prChecks.data.pr;
            checks = prChecks.data.checks;
            comments = prChecks.data.comments;
          }
        }

        return {
          success: true,
          data: { gitStatus, pr, checks, comments },
        };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:merge-pr",
    async (_evt, payload: { taskId: string; prNumber: number; method?: "merge" | "squash" | "rebase" }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const { mergePR } = await import("./orchestrator-pr.js");
        return await mergePR(task.repoPath, payload.prNumber, payload.method ?? "squash");
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Workspace Archiving (Renderer → Main) ────────────────────────────

  ipcMain.handle(
    "orchestrator:archive-task",
    async (_evt, payload: { taskId: string }) => {
      try {
        await orchestrator.archiveTask(payload.taskId);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:restore-task",
    async (_evt, payload: { taskId: string }) => {
      try {
        await orchestrator.restoreTask(payload.taskId);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:delete-permanently",
    async (_evt, payload: { taskId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        await orchestrator.discardTask(task.id);
        // Borrowed workspaces also borrow their branch. Deleting a child
        // must not delete its parent's branch after the worktree is gone.
        if (!task.worktreeMode || task.worktreeMode === "auto") {
          try {
            const { execFile: execFileCb } = await import("node:child_process");
            const { promisify } = await import("node:util");
            const execFileAsync = promisify(execFileCb);
            await execFileAsync("git", ["branch", "-D", task.branch], { cwd: task.repoPath });
          } catch {
            // Branch may already be gone.
          }
        }

        // Commit metadata deletion before removing history. A failed parent/
        // child transaction must leave the original conversation reviewable.
        await orchestrator.removeTask(payload.taskId);
        const { deleteEvents } = await import("./orchestrator-events.js");
        deleteEvents(task.id);
        chat.delete(payload.taskId);

        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Multi-Agent (Renderer → Main) ────────────────────────────────────

  ipcMain.handle(
    "orchestrator:retry-task",
    async (_evt, payload: { taskId: string }) => {
      try {
        await orchestrator.retryTask(payload.taskId);

        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Multi-Agent (Renderer → Main) ────────────────────────────────────

  ipcMain.handle(
    "orchestrator:add-agent",
    async (_evt, payload: { taskId: string; agentType: string; role?: string }) => {
      try {
        const parentTask = orchestrator.getTask(payload.taskId);
        if (!parentTask) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const agentId = randomUUID();
        const prompt = payload.role
          ? `You are the ${payload.role}. ${parentTask.prompt}`
          : parentTask.prompt;
        const childTask = orchestrator.createChildTaskForAgent(
          parentTask.id,
          payload.agentType,
          prompt,
          payload.role,
          agentId,
        );

        await orchestrator.persistTasks();
        startCreatedTask(childTask);
        return { success: true, agentId, childTaskId: childTask.id };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Preferences (Renderer → Main) ────────────────────────────────────

  ipcMain.handle(
    "orchestrator:get-preferences",
    async (_evt, payload: { repoPath: string }) => {
      try {
        const { loadPreferencesFromFile, detectInstructionFiles, loadMCPConfig } = await import("./orchestrator-preferences.js");

        const preferences = loadPreferencesFromFile(payload.repoPath);
        const instructionFiles = detectInstructionFiles(payload.repoPath);
        const mcpServers = loadMCPConfig(payload.repoPath);

        return { success: true, preferences, instructionFiles, mcpServers };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:save-preferences",
    async (_evt, payload: { repoPath: string; preferences: { general: string; codeReview: string; pr: string; conflictResolution: string } }) => {
      try {
        const fs = await import("node:fs");
        const path = await import("node:path");

        const prefsDir = path.join(payload.repoPath, ".crab", "orchestrator");
        fs.mkdirSync(prefsDir, { recursive: true });

        const content = [
          "## General\n",
          payload.preferences.general,
          "\n\n## Code Review\n",
          payload.preferences.codeReview,
          "\n\n## PR\n",
          payload.preferences.pr,
          "\n\n## Conflict Resolution\n",
          payload.preferences.conflictResolution,
        ].join("\n");

        fs.writeFileSync(path.join(prefsDir, "preferences.md"), content, "utf-8");
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Workspace Context & MCP (Renderer → Main) ────────────────────────

  ipcMain.handle(
    "orchestrator:add-context-file",
    async (_evt, payload: { taskId: string; fileName: string; content: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const { addContextFile } = await import("./orchestrator-preferences.js");
        addContextFile(task.worktreePath, payload.fileName, payload.content);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:list-context-files",
    async (_evt, payload: { taskId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const { listContextFiles } = await import("./orchestrator-preferences.js");
        const files = listContextFiles(task.worktreePath);
        return { success: true, files };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:remove-context-file",
    async (_evt, payload: { taskId: string; fileName: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const { removeContextFile } = await import("./orchestrator-preferences.js");
        removeContextFile(task.worktreePath, payload.fileName);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:list-worktree-files",
    async (_evt, payload: { taskId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const { execFile: execFileCb } = await import("node:child_process");
        const { promisify } = await import("node:util");
        const execFileAsync = promisify(execFileCb);

        // List all tracked files + untracked files in the worktree.
        const { stdout } = await execFileAsync(
          "git",
          ["ls-files", "--cached", "--others", "--exclude-standard"],
          { cwd: task.worktreePath, maxBuffer: 10 * 1024 * 1024 },
        );

        const files = stdout
          .split("\n")
          .map((l) => l.trim())
          .filter((l) => l.length > 0);

        return { success: true, files };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:get-mcp-servers",
    async (_evt, payload: { repoPath: string }) => {
      try {
        const { loadMCPConfig } = await import("./orchestrator-preferences.js");
        const servers = loadMCPConfig(payload.repoPath);
        return { success: true, servers };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Cost Tracking (Renderer → Main) ────────────────────────────────────

  ipcMain.handle(
    "orchestrator:get-cost",
    async (_evt, payload: { workspaceId: string }) => {
      try {
        const cost = costTracker.getCost(payload.workspaceId);
        return { success: true, cost: cost ?? null, turns: costTracker.getPerTurnCosts(payload.workspaceId) };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle("orchestrator:get-all-costs", async () => {
    try {
      const costs = costTracker.getAllCosts();
      return { success: true, costs };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle(
    "orchestrator:set-budget",
    async (_evt, payload: { workspaceId: string; limitUsd: number }) => {
      try {
        costTracker.setBudget(payload.workspaceId, payload.limitUsd);
        costTracker.persist();
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle("orchestrator:export-costs-csv", async () => {
    try {
      const csv = costTracker.exportCsv();
      return { success: true, csv };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  // ─── Templates (Renderer → Main) ────────────────────────────────────────

  ipcMain.handle(
    "orchestrator:list-templates",
    async (_evt, payload: { repoPath: string }) => {
      try {
        templateManager.loadTemplates(payload.repoPath);
        const templates = templateManager.listTemplates();
        return { success: true, templates };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:create-from-template",
    async (_evt, payload: { templateName: string; prompt: string; repoPath: string; overrides?: Partial<WorkspaceTemplate> }) => {
      try {
        templateManager.loadTemplates(payload.repoPath);
        const resolved = templateManager.resolveTemplate(payload.templateName, payload.overrides ?? {});
        if (!resolved) {
          return { success: false, error: `Template not found: ${payload.templateName}` };
        }

        let effectivePrompt = payload.prompt;

        // Inject skill content from template skills.
        if (resolved.skills && resolved.skills.length > 0) {
          const { context, errors } = await resolveSkillContext(resolved.skills);
          if (context) {
            effectivePrompt = context + effectivePrompt;
          }
          if (errors.length > 0) {
            console.warn("[orchestrator] skill resolution warnings (template):", errors);
          }
        }

        const baseRef = resolved.baseBranch || "HEAD";

        const task = await orchestrator.createTask(
          effectivePrompt,
          resolved.agentType,
          payload.repoPath,
          baseRef,
        );

        task.planFirst = resolved.planMode;
        task.model = resolved.model;

        // Apply budget limit from template if set.
        if (resolved.budgetLimit) {
          costTracker.setBudget(task.id, resolved.budgetLimit);
          costTracker.persist();
        }

        startCreatedTask(task);

        return { success: true, task };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:save-template",
    async (_evt, payload: WorkspaceTemplate) => {
      try {
        templateManager.saveTemplate(payload);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:delete-template",
    async (_evt, payload: { name: string }) => {
      try {
        const deleted = templateManager.deleteTemplate(payload.name);
        return { success: true, deleted };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Branch Rename (Renderer → Main) ──────────────────────────────────

  ipcMain.handle(
    "orchestrator:list-commands",
    async (_evt, payload: { repoPath: string }) => {
      try {
        commandManager.loadCommands(payload.repoPath);
        const commands = commandManager.listCommands();
        const categories = commandManager.listCategories();
        return { success: true, commands, categories };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:expand-command",
    async (_evt, payload: { name: string; args: Record<string, string> }) => {
      try {
        const expanded = commandManager.expand(payload.name, payload.args);
        return { success: true, expanded };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:create-command",
    async (_evt, payload: { name: string; scope: "repo" | "user"; content: string }) => {
      try {
        return commandManager.createCommand(payload.name, payload.scope, payload.content);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:delete-command",
    async (_evt, payload: { name: string }) => {
      try {
        return commandManager.deleteCommand(payload.name);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:read-command-source",
    async (_evt, payload: { name: string }) => {
      try {
        return commandManager.readCommandSource(payload.name);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:scaffold-command",
    async (_evt, payload: { name: string; description: string; argumentHint?: string; category?: string }) => {
      try {
        const content = commandManager.scaffoldCommand(payload);
        return { success: true, content };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Branch Rename (Renderer → Main) ──────────────────────────────────

  ipcMain.handle(
    "orchestrator:rename-branch",
    async (_evt, payload: { taskId: string; newBranch: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const { execFile: execFileCb } = await import("node:child_process");
        const { promisify } = await import("node:util");
        const execFileAsync = promisify(execFileCb);

        await execFileAsync("git", ["branch", "-m", task.branch, payload.newBranch], { cwd: task.worktreePath });
        task.branch = payload.newBranch;
        await orchestrator.persistTasks();

        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── CI Auto-Fix ─────────────────────────────────────────────────────────

  const autoFixer = new CIAutoFixer(orchestrator);

  autoFixer.on("autofix:attempt", (payload: { taskId: string; attempt: number; maxAttempts: number; failureContext: string }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("orchestrator:autofix-attempt", payload);
    }
  });

  autoFixer.on("autofix:exhausted", (payload: { taskId: string; attempts: number }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("orchestrator:autofix-exhausted", payload);
    }
  });

  autoFixer.on("autofix:success", (payload: { taskId: string; attempts: number }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("orchestrator:autofix-success", payload);
    }
  });

  ipcMain.handle(
    "orchestrator:toggle-autofix",
    async (_evt, payload: { taskId: string; enabled: boolean }) => {
      try {
        autoFixer.toggle(payload.taskId, payload.enabled);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Workspace Search (Renderer → Main) ────────────────────────────────

  const workspaceSearch = new WorkspaceSearch();

  ipcMain.handle(
    "orchestrator:search",
    async (_evt, payload: { query: string; filters?: SearchFilters }) => {
      try {
        const tasks = orchestrator.getAllTasks();
        workspaceSearch.buildIndex(tasks);
        const results = workspaceSearch.search(payload.query, payload.filters);
        return { success: true, results };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // ─── Chat Persistence (SQLite) ──────────────────────────────────────

  ipcMain.handle(
    "orchestrator:search-chat",
    async (_evt, payload: { query: string; taskId?: string; limit?: number }) => {
      try {
        const results = payload.taskId
          ? chatDb.searchInTask(payload.taskId, payload.query, payload.limit ?? 50)
          : chatDb.search(payload.query, payload.limit ?? 50);
        return { success: true, results };
      } catch (err) {
        return { success: false, error: err instanceof Error ? err.message : String(err) };
      }
    },
  );

  // ─── Auto-Archive (check merged PRs) ──────────────────────────────────

  // Check every 5 minutes for merged PRs that should be auto-archived
  setInterval(async () => {
    const tasks = orchestrator.getAllTasks();
    const now = Date.now();
    const autoArchiveDelay = 24 * 60 * 60 * 1000; // 24 hours

    for (const task of tasks) {
      if (task.status === "archived") continue;
      const prNumber = task.prNumber;
      if (!prNumber) continue;

      // Check if PR is merged and enough time has passed
      if (task.completedAt && now - task.completedAt > autoArchiveDelay) {
        try {
          const { getPRChecks } = await import("./orchestrator-pr.js");
          const result = await getPRChecks(task.repoPath, prNumber);
          if (result.success && result.data?.pr?.state === "MERGED") {
            await orchestrator.archiveTask(task.id);
          }
        } catch {
          // Best-effort
        }
      }
    }
  }, 5 * 60 * 1000);

  // ─── Thread Operations (Codex app-server) ─────────────────────────

  ipcMain.handle("orchestrator:fork-thread", async (_evt, payload: { taskId: string }) => {
    try {
      const result = await orchestrator.forkThread(payload.taskId);
      return { success: true, ...result };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:compact-thread", async (_evt, payload: { taskId: string }) => {
    try {
      await orchestrator.compactThread(payload.taskId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:start-review", async (_evt, payload: { taskId: string }) => {
    try {
      await orchestrator.startReview(payload.taskId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:set-thread-name", async (_evt, payload: { taskId: string; name: string }) => {
    try {
      await orchestrator.setThreadName(payload.taskId, payload.name);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:set-goal", async (_evt, payload: { taskId: string; mutation: import("./orchestrator-adapters").CodexGoalMutation }) => {
    try {
      const goal = await orchestrator.setGoal(payload.taskId, payload.mutation);
      return { success: true, goal };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:get-goal", async (_evt, payload: { taskId: string }) => {
    try {
      const goal = await orchestrator.getGoal(payload.taskId);
      return { success: true, goal };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:clear-goal", async (_evt, payload: { taskId: string }) => {
    try {
      await orchestrator.clearGoal(payload.taskId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:update-thread-metadata", async (_evt, payload: { taskId: string; metadata: Record<string, unknown> }) => {
    try {
      await orchestrator.updateThreadMetadata(payload.taskId, payload.metadata);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:shell-command", async (_evt, payload: { taskId: string; command: string; cwd?: string }) => {
    try {
      const result = await orchestrator.shellCommand(payload.taskId, payload.command, payload.cwd);
      return { success: true, ...result };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:clean-background-terminals", async (_evt, payload: { taskId: string }) => {
    try {
      await orchestrator.cleanBackgroundTerminals(payload.taskId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:inject-items", async (_evt, payload: { taskId: string; items: Array<{ role: string; content: unknown }> }) => {
    try {
      await orchestrator.injectItems(payload.taskId, payload.items);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  // ─── Command Execution (Codex app-server) ─────────────────────────────

  ipcMain.handle("orchestrator:exec-command", async (_evt, payload: { taskId: string; command: string; args?: string[]; cwd?: string; env?: Record<string, string>; stdin?: string; timeoutMs?: number }) => {
    try {
      const result = await orchestrator.execCommand(payload.taskId, {
        command: payload.command,
        args: payload.args,
        cwd: payload.cwd,
        env: payload.env,
        stdin: payload.stdin,
        timeoutMs: payload.timeoutMs,
      });
      return { success: true, ...result };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:write-command-stdin", async (_evt, payload: { taskId: string; sessionId: string; stdin: string; closeStdin?: boolean }) => {
    try {
      await orchestrator.writeCommandStdin(payload.taskId, payload.sessionId, payload.stdin, payload.closeStdin);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:resize-command-pty", async (_evt, payload: { taskId: string; sessionId: string; cols: number; rows: number }) => {
    try {
      await orchestrator.resizeCommandPty(payload.taskId, payload.sessionId, payload.cols, payload.rows);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:terminate-command", async (_evt, payload: { taskId: string; sessionId: string }) => {
    try {
      await orchestrator.terminateCommand(payload.taskId, payload.sessionId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  // ─── Feedback (Codex app-server) ──────────────────────────────────────

  ipcMain.handle("orchestrator:upload-feedback", async (_evt, payload: { taskId: string; turnId?: string; rating?: number; comment?: string; tags?: string[] }) => {
    try {
      await orchestrator.uploadFeedback(payload.taskId, {
        turnId: payload.turnId,
        rating: payload.rating,
        comment: payload.comment,
        tags: payload.tags,
      });
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  // ─── Filesystem (Codex app-server) ─────────────────────────────────────

  ipcMain.handle("orchestrator:fs-read-file", async (_evt, payload: { path: string; taskId?: string }) => {
    try {
      const content = await orchestrator.fsReadFile(payload.path, payload.taskId);
      return { success: true, content };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:fs-write-file", async (_evt, payload: { path: string; content: string; taskId?: string }) => {
    try {
      await orchestrator.fsWriteFile(payload.path, payload.content, payload.taskId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:fs-create-directory", async (_evt, payload: { path: string; taskId?: string }) => {
    try {
      await orchestrator.fsCreateDirectory(payload.path, payload.taskId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:fs-get-metadata", async (_evt, payload: { path: string; taskId?: string }) => {
    try {
      const meta = await orchestrator.fsGetMetadata(payload.path, payload.taskId);
      return { success: true, ...meta };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:fs-read-directory", async (_evt, payload: { path: string; taskId?: string }) => {
    try {
      const entries = await orchestrator.fsReadDirectory(payload.path, payload.taskId);
      return { success: true, entries };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:fs-remove", async (_evt, payload: { path: string; recursive?: boolean; taskId?: string }) => {
    try {
      await orchestrator.fsRemove(payload.path, payload.recursive, payload.taskId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:fs-copy", async (_evt, payload: { source: string; destination: string; taskId?: string }) => {
    try {
      await orchestrator.fsCopy(payload.source, payload.destination, payload.taskId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:fs-watch", async (_evt, payload: { path: string; taskId?: string }) => {
    try {
      const watchId = await orchestrator.fsWatch(payload.path, payload.taskId);
      return { success: true, watchId };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:fs-unwatch", async (_evt, payload: { watchId: string; taskId?: string }) => {
    try {
      await orchestrator.fsUnwatch(payload.watchId, payload.taskId);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  // ─── Command / Filesystem Event Relay ──────────────────────────────────

  orchestrator.on("orchestrator:event", (payload: { taskId: string; event: unknown }) => {
    const win = getMainWindow();
    if (!win || win.isDestroyed()) return;
    const event = payload.event as Record<string, unknown> | undefined;
    if (!event) return;

    if (event.type === "command_output") {
      win.webContents.send("orchestrator:command-output", {
        taskId: payload.taskId,
        sessionId: event.commandSessionId,
        delta: event.content,
      });
    }
    if (event.type === "command_exited") {
      win.webContents.send("orchestrator:command-exited", {
        taskId: payload.taskId,
        sessionId: event.commandSessionId,
        exitCode: event.exitCode,
      });
    }
    if (event.type === "filesystem_change") {
      win.webContents.send("orchestrator:fs-changed", {
        taskId: payload.taskId,
        watchId: event.fsWatchId,
        path: event.path,
        kind: event.fsChangeKind,
      });
    }
  });

  return {
    orchestrator,
    shutdown: async () => {
      const results = await Promise.allSettled([orchestrator.shutdown(), stopModelDiscovery()]);
      const errors = results.filter((result): result is PromiseRejectedResult => result.status === "rejected");
      if (errors.length) throw new AggregateError(errors.map(result => result.reason), "Agent services shutdown failed");
    },
  };
}
