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

import { Orchestrator } from "./orchestrator";
import type { PersistedChatUIState } from "./orchestrator";
import { ChatDatabase } from "./orchestrator-chat-db";
import { ensureInitialPromptMessage, type ChatHistoryMessage } from "./orchestrator-chat-history";
import { computeDiff, computeRemoteDiff, applyAll, cherryPickFiles } from "./orchestrator-diff";
import { detectAgents, getCachedAgents, prewarmAgents } from "./orchestrator-adapters";
import { fetchGitHubIssues, fetchLinearIssues } from "./orchestrator-issues";
import { CheckpointManager } from "./orchestrator-checkpoints";
import { CostTracker, TokenUsage } from "./orchestrator-cost";
import { agentRegistry } from "./orchestrator-registry";
import { loadAgentConfig, saveAgentConfig } from "./orchestrator-settings";
import {
  loadScripts,
  buildWorkspaceEnv,
  runSetupScript,
  runArchiveScript,
  spawnRunScript,
  allocatePort,
} from "./orchestrator-scripts";
import { TemplateManager, WorkspaceTemplate } from "./orchestrator-templates";
import { SlashCommandManager } from "./orchestrator-commands";
import { WorkspaceSearch } from "./orchestrator-search";
import type { SearchFilters } from "./orchestrator-search";
import { CIAutoFixer } from "./orchestrator-autofix";
import { applyHunk, revertHunk, type HunkPatch } from "./orchestrator-hunks";
import type { SkillManager } from "./skill-manager";

const IPC_GIT_DIFF_TIMEOUT_MS = 10_000;

function isRenderableAgentText(msg: ChatHistoryMessage): boolean {
  return msg.role === "agent" && (msg.kind === "text" || msg.kind === "done" || msg.kind === undefined);
}

function dedupeLoadedChatMessages(messages: ChatHistoryMessage[]): ChatHistoryMessage[] {
  const deduped: ChatHistoryMessage[] = [];

  for (const msg of messages) {
    const content = msg.content.trim();
    if (content && isRenderableAgentText(msg)) {
      let duplicateIdx = -1;
      for (let i = deduped.length - 1; i >= 0; i--) {
        const previous = deduped[i];
        if (previous.role === "user") break;
        if (!isRenderableAgentText(previous)) continue;
        if (previous.content.trim() === content) {
          duplicateIdx = i;
          break;
        }
      }

      if (duplicateIdx >= 0) {
        const previous = deduped[duplicateIdx];
        const previousEventCount = previous.events?.length ?? 0;
        const incomingEventCount = msg.events?.length ?? 0;
        if (
          incomingEventCount > previousEventCount ||
          (previousEventCount === 0 && incomingEventCount === 0 && previous.kind !== "done")
        ) {
          deduped[duplicateIdx] = msg;
        }
        continue;
      }
    }

    deduped.push(msg);
  }

  return deduped;
}

/**
 * Registers all orchestrator IPC handlers on the given `ipcMain` instance.
 *
 * Creates an `Orchestrator` instance, wires renderer → main command handlers,
 * and relays orchestrator events to the renderer via `webContents.send()`.
 *
 * Returns the orchestrator instance so callers (e.g. the SSH subsystem) can
 * wire additional event sources like the RPC router's notification stream.
 */
export function registerOrchestratorIpc(
  ipcMain: IpcMain,
  getMainWindow: () => BrowserWindow | null,
  skillManager?: SkillManager,
): Orchestrator {
  const orchestrator = new Orchestrator();

  // ─── Event Relay (Main → Renderer) ──────────────────────────────────────

  orchestrator.on("orchestrator:event", (payload: { taskId: string; event: unknown }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("orchestrator:event", payload);
    }
  });

  // ─── Main-process chat persistence ─────────────────────────────────────
  // Persist text and done events to the chat DB from the main process so
  // messages are available even if the renderer is unmounted (e.g. user
  // navigated away during streaming). Without this, events emitted while
  // the renderer isn't listening are lost and the user sees a blank page
  // on return.
  //
  // Strategy: accumulate streaming text per task. On `done`, check if the
  // renderer has already saved messages for this task recently (within the
  // last 3s). If so, skip — the renderer's save is authoritative. If not,
  // persist the accumulated text as a fallback.
  const mainProcessChatBuffer = new Map<string, { content: string; timestamp: number }>();
  const lastRendererSave = new Map<string, number>();

  orchestrator.on("orchestrator:event", (payload: { taskId: string; event: any }) => {
    const { taskId, event } = payload;
    if (!event?.type) return;

    if (event.type === "text" && event.content) {
      // Accumulate streaming text for this task.
      const existing = mainProcessChatBuffer.get(taskId);
      if (existing && Date.now() - existing.timestamp < 2000) {
        existing.content += event.content;
        existing.timestamp = Date.now();
      } else {
        mainProcessChatBuffer.set(taskId, { content: event.content, timestamp: Date.now() });
      }
    }

    if (event.type === "done") {
      // On turn completion, persist the accumulated text to the chat DB
      // ONLY if the renderer hasn't saved recently (i.e. it was unmounted).
      const buffered = mainProcessChatBuffer.get(taskId);
      const content = event.content || buffered?.content || "";
      mainProcessChatBuffer.delete(taskId);

      const lastSave = lastRendererSave.get(taskId) ?? 0;
      const rendererIsActive = Date.now() - lastSave < 3000;

      if (content && !rendererIsActive) {
        try {
          const msgId = `${taskId}-main-${Date.now()}-${Math.random().toString(36).slice(2)}`;
          chatDb.insertMessage({
            id: msgId,
            task_id: taskId,
            role: "agent",
            kind: "done",
            content,
            events: null,
            timestamp: event.timestamp ?? Date.now(),
          });
        } catch {
          // Best-effort persistence.
        }
      }
    }
  });

  orchestrator.on("orchestrator:task-status", (payload: { taskId: string; status: string }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("orchestrator:task-status", payload);
    }
  });

  orchestrator.on("orchestrator:error", (payload: { taskId: string | null; message: string }) => {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("orchestrator:error", payload);
    }
  });

  orchestrator.on(
    "orchestrator:iteration-warning",
    (payload: { taskId: string; iterations: number; message: string }) => {
      const win = getMainWindow();
      if (win && !win.isDestroyed()) {
        win.webContents.send("orchestrator:iteration-warning", payload);
      }
    },
  );

  orchestrator.on(
    "orchestrator:checkpoint-created",
    (payload: { taskId: string; turnNumber: number; ref: string; sha: string }) => {
      const win = getMainWindow();
      if (win && !win.isDestroyed()) {
        win.webContents.send("orchestrator:checkpoint-created", payload);
      }
    },
  );

  orchestrator.on(
    "orchestrator:nonconcurrent-stop",
    (payload: { stoppedTaskId: string; startedTaskId: string }) => {
      const win = getMainWindow();
      if (win && !win.isDestroyed()) {
        win.webContents.send("orchestrator:nonconcurrent-stop", payload);
      }
    },
  );

  // ─── Checkpoint Manager ─────────────────────────────────────────────────

  const checkpointManager = new CheckpointManager();

  // ─── Template Manager ─────────────────────────────────────────────────────

  const templateManager = new TemplateManager();

  // ─── Slash Command Manager ──────────────────────────────────────────────

  const commandManager = new SlashCommandManager();

  // ─── Cost Tracker ───────────────────────────────────────────────────────

  const costTracker = new CostTracker();
  costTracker.load();

  // When a usage event arrives from an adapter, record it in the cost tracker.
  orchestrator.on("orchestrator:event", (payload: { taskId: string; event: { type: string; usage?: { inputTokens: number; outputTokens: number; cacheCreationInputTokens: number; cacheReadInputTokens: number } } }) => {
    if (payload.event.type !== "usage" || !payload.event.usage) return;

    const task = orchestrator.getTask(payload.taskId);
    if (!task) return;

    const usage: TokenUsage = {
      inputTokens: payload.event.usage.inputTokens,
      outputTokens: payload.event.usage.outputTokens,
      cacheReadTokens: payload.event.usage.cacheReadInputTokens,
      cacheWriteTokens: payload.event.usage.cacheCreationInputTokens,
      timestamp: Date.now(),
      turnId: `${payload.taskId}-${Date.now()}`,
    };

    costTracker.recordUsage(payload.taskId, usage, task.agentType, "");
    costTracker.persist();

    // Relay budget warnings to the renderer.
    const budgetCheck = costTracker.checkBudget(payload.taskId);
    if (budgetCheck.exceeded) {
      const win = getMainWindow();
      if (win && !win.isDestroyed()) {
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

        // When plan mode is enabled, prefix the prompt with planning instructions.
        let effectivePrompt = payload.prompt;
        if (payload.planFirst) {
          effectivePrompt =
            "First, propose an implementation plan. List files to change, approach, and risks. Wait for approval before making changes.\n\nTask: " +
            payload.prompt;
        }

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
        orchestrator.startTask(task.id).then(() => {
          console.log("[orchestrator] task started:", task.id);
        }).catch((err: unknown) => {
          console.error("[orchestrator] task start failed:", task.id, err);
        });

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

  ipcMain.handle(
    "orchestrator:iterate",
    async (_evt, payload: { taskId: string; feedback: string }) => {
      try {
        await orchestrator.iterate(payload.taskId, payload.feedback);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:approve-plan",
    async (_evt, payload: { taskId: string }) => {
      try {
        await orchestrator.iterate(payload.taskId, "Plan approved. Proceed with implementation.");
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  // Records the user's response to a Codex sandbox approval request.
  // The decision is stashed on the task and consumed by the next
  // `iterate()` call as a "User decisions:" preamble. We do not
  // re-spawn the agent here — `codex exec` is single-turn and the
  // running process is already blocked or has surfaced the prompt.
  ipcMain.handle(
    "orchestrator:respond-approval",
    async (_evt, payload: { taskId: string; requestId: string; decision: "allow" | "allow_once" | "deny"; summary: string; note?: string }) => {
      try {
        const ok = await orchestrator.recordApproval(payload.taskId, {
          requestId: payload.requestId,
          decision: payload.decision,
          summary: payload.summary,
          note: payload.note,
        });
        if (!ok) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

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

  ipcMain.handle(
    "orchestrator:apply-all",
    async (_evt, payload: { taskId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        // Remote tasks: the orchestrator.applyTask() handles the merge
        // on the remote machine via RPC. No local git operations needed.
        if (task.isRemote) {
          await orchestrator.applyTask(payload.taskId);
          return { success: true };
        }

        const result = await applyAll(task);
        if (result.success) {
          await orchestrator.applyTask(payload.taskId);
        }
        return { success: result.success, conflicts: result.conflicts };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:cherry-pick",
    async (_evt, payload: { taskId: string; files: string[] }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        await cherryPickFiles(task, payload.files);
        await orchestrator.applyTask(payload.taskId);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:discard",
    async (_evt, payload: { taskId: string; archiveScript?: string; repoPath?: string; portRangeStart?: number; portRangeEnd?: number }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (task && payload.archiveScript?.trim()) {
          // Run archive script before discarding.
          const port = allocatedPorts.get(payload.taskId) ?? 3100;
          const env = buildWorkspaceEnv(
            task.worktreePath,
            task.branch,
            task.repoPath,
            port,
          );
          await runArchiveScript(payload.archiveScript, task.worktreePath, env);
        }

        // Release allocated port.
        allocatedPorts.delete(payload.taskId);

        // Stop any running process.
        const proc = runningProcesses.get(payload.taskId);
        if (proc && !proc.killed) {
          proc.kill("SIGTERM");
          runningProcesses.delete(payload.taskId);
        }
        orchestrator.clearActiveRunTaskId(payload.taskId);

        await orchestrator.discardTask(payload.taskId);
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

  ipcMain.handle(
    "orchestrator:get-diff",
    async (_evt, payload: { taskId: string; checkpointSha?: string; checkpointFromSha?: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        // If a checkpoint SHA is provided, compute diff for that specific turn.
        let diff;
        if (payload.checkpointSha) {
          const { execFile: execFileCb } = await import("node:child_process");
          const { promisify } = await import("node:util");
          const execFileAsync = promisify(execFileCb);

          // "from" is either the previous checkpoint SHA or the task's baseRef.
          const fromRef = payload.checkpointFromSha || task.baseRef;
          const isLatestTurn = payload.checkpointSha === "HEAD";

          // For the latest turn, agents commonly leave new files uncommitted.
          // A plain `git diff <fromSha> HEAD` misses those entirely, which is
          // what makes time-travel on the newest node look broken. Build a
          // live snapshot of the working tree (same shape as a real
          // checkpoint) and diff against that so untracked + unstaged edits
          // show up just like committed ones.
          let toRef: string;
          if (isLatestTurn) {
            toRef = await checkpointManager.snapshotHead(task);
            if (!toRef) {
              return { success: false, error: "Could not snapshot current worktree" };
            }
          } else {
            toRef = payload.checkpointSha;
          }

          // Use the worktree as primary cwd since that's where snapshots
          // are created; fall back to repoPath if the worktree is gone.
          const cwd = (task.worktreePath && fs.existsSync(task.worktreePath))
            ? task.worktreePath
            : task.repoPath;
          let rawDiff = "";
          let nameStatus = "";

          // Resolve both refs to tree SHAs for a reliable tree-to-tree diff.
          // Snapshot commits may share ancestry that confuses plain `git diff
          // <commit> <commit>` in some git configurations. Using `git diff-tree`
          // with explicit tree SHAs is more reliable.
          let fromTree = "";
          let toTree = "";
          try {
            const { stdout: ft } = await execFileAsync(
              "git", ["rev-parse", `${fromRef}^{tree}`], { cwd, timeout: IPC_GIT_DIFF_TIMEOUT_MS, killSignal: "SIGTERM" },
            );
            fromTree = ft.trim();
            const { stdout: tt } = await execFileAsync(
              "git", ["rev-parse", `${toRef}^{tree}`], { cwd, timeout: IPC_GIT_DIFF_TIMEOUT_MS, killSignal: "SIGTERM" },
            );
            toTree = tt.trim();
          } catch {
            // Fall back to using commit SHAs directly if tree resolution fails.
            fromTree = "";
            toTree = "";
          }

          // Use tree SHAs if available (more reliable), otherwise commit SHAs.
          const diffFrom = fromTree || fromRef;
          const diffTo = toTree || toRef;

          try {
            const result = await execFileAsync(
              "git", ["diff", diffFrom, diffTo],
              { cwd, maxBuffer: 50 * 1024 * 1024, timeout: IPC_GIT_DIFF_TIMEOUT_MS, killSignal: "SIGTERM" },
            );
            rawDiff = result.stdout;
            const nsResult = await execFileAsync(
              "git", ["diff", "--name-status", diffFrom, diffTo],
              { cwd, timeout: IPC_GIT_DIFF_TIMEOUT_MS, killSignal: "SIGTERM" },
            );
            nameStatus = nsResult.stdout;
          } catch {
            // Try the other cwd as a fallback.
            const altCwd = cwd === task.worktreePath ? task.repoPath : (task.worktreePath || task.repoPath);
            try {
              const result = await execFileAsync(
                "git", ["diff", diffFrom, diffTo],
                { cwd: altCwd, maxBuffer: 50 * 1024 * 1024, timeout: IPC_GIT_DIFF_TIMEOUT_MS, killSignal: "SIGTERM" },
              );
              rawDiff = result.stdout;
              const nsResult = await execFileAsync(
                "git", ["diff", "--name-status", diffFrom, diffTo],
                { cwd: altCwd, timeout: IPC_GIT_DIFF_TIMEOUT_MS, killSignal: "SIGTERM" },
              );
              nameStatus = nsResult.stdout;
            } catch {
              return { success: false, error: "Could not compute diff at checkpoint" };
            }
          }

          const { parseDiffOutput } = await import("./orchestrator-diff.js");
          const files = parseDiffOutput(rawDiff, nameStatus);
          diff = {
            taskId: task.id,
            baseRef: fromRef,
            // Expose "HEAD" to callers for the latest turn so the UI can tell
            // this is a live snapshot; internally we already diffed against
            // the real snapshot SHA above.
            headRef: isLatestTurn ? "HEAD" : toRef,
            files,
            totalAdditions: files.reduce((s, f) => s + f.additions, 0),
            totalDeletions: files.reduce((s, f) => s + f.deletions, 0),
            rawDiff,
          };
        } else {
          // For remote tasks, route diff computation to the remote agent.
          if (task.isRemote && task.remoteProfileId) {
            const router = orchestrator.getRpcRouter();
            if (!router) {
              return { success: false, error: "SSH router not configured for remote diff." };
            }
            diff = await computeRemoteDiff(task, router);
          } else {
            diff = await computeDiff(task);
          }
        }

        // Also relay as a diff-ready event for any listeners.
        const win = getMainWindow();
        if (win && !win.isDestroyed()) {
          win.webContents.send("orchestrator:diff-ready", {
            taskId: task.id,
            diff: diff.rawDiff,
            files: diff.files,
          });
        }
        return { success: true, diff };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:review-with-ai",
    async (_evt, payload: { taskId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        let diff;
        if (task.isRemote && task.remoteProfileId) {
          const router = orchestrator.getRpcRouter();
          if (!router) {
            return { success: false, error: "SSH router not configured for remote diff." };
          }
          diff = await computeRemoteDiff(task, router);
        } else {
          diff = await computeDiff(task);
        }
        if (!diff.rawDiff) {
          return { success: false, error: "No diff found for this task." };
        }

        // Build the review prompt.
        const reviewPrompt = buildReviewPrompt(diff.rawDiff, task.prompt);

        // Spawn the same agent CLI that ran the task in one-shot mode
        // to perform the review. This avoids needing a separate AI provider.
        const { getExpandedEnv } = await import("./env/userShellEnv.js");
        const { execFile: execFileCb } = await import("node:child_process");
        const { promisify } = await import("node:util");
        const execFileAsync = promisify(execFileCb);

        const agentConfig = loadAgentConfig(task.agentType);
        const agentEnv = await getExpandedEnv(agentConfig.env);

        let reviewText: string;

        if (task.agentType === "codex") {
          // Codex: `codex exec <prompt>` (no --json for plain text output)
          const binary = agentConfig.binaryPath || "codex";
          const { stdout } = await execFileAsync(
            binary,
            ["exec", "--sandbox", "none", reviewPrompt],
            { cwd: task.worktreePath, env: agentEnv, timeout: 120_000, maxBuffer: 2 * 1024 * 1024 },
          );
          reviewText = stdout.trim();
        } else {
          // Claude Code (default): `claude --print <prompt>`
          const binary = agentConfig.binaryPath || "claude";
          const { stdout } = await execFileAsync(
            binary,
            ["--print", reviewPrompt],
            { cwd: task.worktreePath, env: agentEnv, timeout: 120_000, maxBuffer: 2 * 1024 * 1024 },
          );
          reviewText = stdout.trim();
        }

        if (!reviewText) {
          return { success: false, error: "Agent returned an empty review." };
        }

        return { success: true, reviewText };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:revert-file",
    async (_evt, payload: { taskId: string; filePath: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        // Revert a single file in the worktree to the base ref state.
        const { execFile: execFileCb } = await import("node:child_process");
        const { promisify } = await import("node:util");
        const execFile = promisify(execFileCb);
        await execFile(
          "git",
          ["checkout", task.baseRef, "--", payload.filePath],
          { cwd: task.worktreePath },
        );
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

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

  // ─── Workspace Scripts (Renderer → Main) ──────────────────────────────

  /** Tracks allocated ports per workspace (taskId → port). */
  const allocatedPorts = new Map<string, number>();
  /** Tracks running workspace processes (taskId → ChildProcess). */
  const runningProcesses = new Map<string, import("node:child_process").ChildProcess>();

  ipcMain.handle(
    "orchestrator:run-setup-script",
    async (_evt, payload: { taskId: string; repoPath: string; setupScript?: string; runScript?: string; archiveScript?: string; portRangeStart?: number; portRangeEnd?: number }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        const settingsScripts = {
          setupScript: payload.setupScript ?? "",
          runScript: payload.runScript ?? "",
          archiveScript: payload.archiveScript ?? "",
        };

        const scripts = loadScripts(payload.repoPath, settingsScripts);

        if (!scripts.setupScript.trim()) {
          return { success: true, output: "", skipped: true };
        }

        // Allocate a port for this workspace.
        const usedPorts = new Set(allocatedPorts.values());
        const port = allocatePort(
          usedPorts,
          payload.portRangeStart ?? 3100,
          payload.portRangeEnd ?? 3199,
        );
        if (port !== -1) {
          allocatedPorts.set(payload.taskId, port);
        }

        const env = buildWorkspaceEnv(
          task.worktreePath,
          task.branch,
          task.repoPath,
          port !== -1 ? port : 3100,
        );

        const win = getMainWindow();
        const result = await runSetupScript(
          scripts.setupScript,
          task.worktreePath,
          env,
          (data) => {
            if (win && !win.isDestroyed()) {
              win.webContents.send("orchestrator:event", {
                taskId: payload.taskId,
                event: { type: "command_run", timestamp: Date.now(), stdout: data, command: "setup-script" },
              });
            }
          },
        );

        return { success: result.success, output: result.output, exitCode: result.exitCode, error: result.error };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:run-workspace",
    async (_evt, payload: { taskId: string; repoPath: string; command?: string; setupScript?: string; runScript?: string; archiveScript?: string; portRangeStart?: number; portRangeEnd?: number }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        // Load nonconcurrent run setting and stop the active run if needed.
        orchestrator.loadNonconcurrentRunSetting(payload.repoPath);
        const stoppedTaskId = await orchestrator.stopActiveRunIfNeeded(
          payload.taskId,
          runningProcesses,
        );

        if (stoppedTaskId) {
          const win = getMainWindow();
          if (win && !win.isDestroyed()) {
            win.webContents.send("orchestrator:nonconcurrent-stop", {
              stoppedTaskId,
              startedTaskId: payload.taskId,
            });
          }
        }

        // Stop any existing process for this workspace.
        const existing = runningProcesses.get(payload.taskId);
        if (existing && !existing.killed) {
          existing.kill("SIGTERM");
          runningProcesses.delete(payload.taskId);
        }

        const settingsScripts = {
          setupScript: payload.setupScript ?? "",
          runScript: payload.runScript ?? "",
          archiveScript: payload.archiveScript ?? "",
        };

        const scripts = loadScripts(payload.repoPath, settingsScripts);
        const command = payload.command ?? scripts.runScript;

        if (!command.trim()) {
          return { success: false, error: "No run script configured" };
        }

        // Ensure port is allocated.
        if (!allocatedPorts.has(payload.taskId)) {
          const usedPorts = new Set(allocatedPorts.values());
          const port = allocatePort(
            usedPorts,
            payload.portRangeStart ?? 3100,
            payload.portRangeEnd ?? 3199,
          );
          if (port !== -1) {
            allocatedPorts.set(payload.taskId, port);
          }
        }

        const port = allocatedPorts.get(payload.taskId) ?? 3100;
        const env = buildWorkspaceEnv(task.worktreePath, task.branch, task.repoPath, port);

        const proc = spawnRunScript(command, task.worktreePath, env);
        if (!proc) {
          return { success: false, error: "Failed to spawn run script" };
        }

        runningProcesses.set(payload.taskId, proc);

        // Track this as the active run for nonconcurrent mode.
        orchestrator.setActiveRunTaskId(payload.taskId);

        const win = getMainWindow();
        proc.stdout?.on("data", (chunk: Buffer) => {
          if (win && !win.isDestroyed()) {
            win.webContents.send("orchestrator:terminal-data", {
              taskId: payload.taskId,
              data: chunk.toString(),
            });
          }
        });

        proc.stderr?.on("data", (chunk: Buffer) => {
          if (win && !win.isDestroyed()) {
            win.webContents.send("orchestrator:terminal-data", {
              taskId: payload.taskId,
              data: chunk.toString(),
            });
          }
        });

        proc.on("close", (code) => {
          runningProcesses.delete(payload.taskId);
          orchestrator.clearActiveRunTaskId(payload.taskId);
          if (win && !win.isDestroyed()) {
            win.webContents.send("orchestrator:terminal-exit", {
              taskId: payload.taskId,
              exitCode: code ?? 0,
            });
          }
        });

        return { success: true, port, stoppedTaskId };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:stop-workspace",
    async (_evt, payload: { taskId: string }) => {
      try {
        const proc = runningProcesses.get(payload.taskId);
        if (!proc || proc.killed) {
          return { success: true };
        }

        // Send SIGHUP, wait 200ms, then SIGKILL if still running.
        proc.kill("SIGHUP");
        await new Promise<void>((resolve) => {
          const timer = setTimeout(() => {
            if (!proc.killed) {
              proc.kill("SIGKILL");
            }
            resolve();
          }, 200);

          proc.on("close", () => {
            clearTimeout(timer);
            resolve();
          });
        });

        runningProcesses.delete(payload.taskId);
        orchestrator.clearActiveRunTaskId(payload.taskId);
        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

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

  const resolveHunkPath = (
    task: { worktreePath: string; repoPath: string },
    filePath: string,
  ): string => {
    if (!filePath) throw new Error("Missing file path for hunk operation");
    const absolute = path.isAbsolute(filePath)
      ? filePath
      : path.resolve(task.worktreePath, filePath);
    const worktree = path.resolve(task.worktreePath);
    if (!absolute.startsWith(worktree + path.sep) && absolute !== worktree) {
      throw new Error(`File ${filePath} is outside the task worktree`);
    }
    return absolute;
  };

  ipcMain.handle(
    "orchestrator:apply-hunk",
    async (_evt, payload: { taskId: string; patch: HunkPatch }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        const resolvedPath = resolveHunkPath(task, payload.patch.filePath);
        await applyHunk({ ...payload.patch, filePath: resolvedPath });
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
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        const resolvedPath = resolveHunkPath(task, payload.patch.filePath);
        await revertHunk({ ...payload.patch, filePath: resolvedPath });
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
        const checkpoints = await checkpointManager.listCheckpoints(task);
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
    "orchestrator:revert-checkpoint",
    async (_evt, payload: { taskId: string; turnNumber: number }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        if (task.worktreeMode === "primary") {
          return {
            success: false,
            error: "Checkpoint revert is disabled for primary-folder tasks because it would reset the main working directory.",
          };
        }
        await checkpointManager.revertToCheckpoint(task, payload.turnNumber);
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
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }
        const checkpoints = await checkpointManager.listCheckpoints(task);
        return { success: true, checkpoints };
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
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        // Persist events before archiving
        const { persistEvents } = await import("./orchestrator-events.js");
        await persistEvents(task.id, task.events);

        // Remove worktree but preserve branch + metadata
        try {
          const { removeWorktree } = await import("./orchestrator-worktree.js");
          await removeWorktree(task.repoPath, task.id);
        } catch {
          // Worktree may already be gone
        }

        (task as any).status = "archived" as any;
        task.completedAt = Date.now();
        await orchestrator.persistTasks();

        const win = getMainWindow();
        if (win && !win.isDestroyed()) {
          win.webContents.send("orchestrator:task-status", { taskId: task.id, status: "archived" });
        }

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
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        // Re-create worktree from preserved branch
        const { createWorktree } = await import("./orchestrator-worktree.js");
        const worktreePath = await createWorktree(task.repoPath, task.id, task.branch);
        task.worktreePath = worktreePath;
        (task as any).status = "reviewing" as any;
        await orchestrator.persistTasks();

        const win = getMainWindow();
        if (win && !win.isDestroyed()) {
          win.webContents.send("orchestrator:task-status", { taskId: task.id, status: "reviewing" });
        }

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

        // Remove branch
        try {
          const { execFile: execFileCb } = await import("node:child_process");
          const { promisify } = await import("node:util");
          const execFileAsync = promisify(execFileCb);
          await execFileAsync("git", ["branch", "-D", task.branch], { cwd: task.repoPath });
        } catch {
          // Branch may already be gone
        }

        // Remove persisted events
        const { deleteEvents } = await import("./orchestrator-events.js");
        deleteEvents(task.id);

        // Remove task from orchestrator
        await orchestrator.removeTask(payload.taskId);

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
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        // Reset the task to "created" and re-start it.
        (task as any).status = "created";
        task.completedAt = undefined;
        task.exitCode = undefined;
        await orchestrator.persistTasks();
        await orchestrator.startTask(payload.taskId);

        return { success: true };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return { success: false, error: message };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:undo-apply",
    async (_evt, payload: { taskId: string }) => {
      try {
        const task = orchestrator.getTask(payload.taskId);
        if (!task) {
          return { success: false, error: `Task not found: ${payload.taskId}` };
        }

        // Revert the task back to "reviewing" so the user can re-evaluate.
        (task as any).status = "reviewing";
        task.completedAt = undefined;
        await orchestrator.persistTasks();

        const win = getMainWindow();
        if (win && !win.isDestroyed()) {
          win.webContents.send("orchestrator:task-status", { taskId: task.id, status: "reviewing" });
        }

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

        const { createAdapter } = await import("./orchestrator-adapters.js");
        const agentId = `agent-${Date.now().toString(36)}`;
        const adapter = createAdapter(payload.agentType as any);

        const prompt = payload.role
          ? `You are the ${payload.role}. ${parentTask.prompt}`
          : parentTask.prompt;
        const childTask = orchestrator.createChildTaskForAgent(
          parentTask.id,
          payload.agentType as any,
          prompt,
          payload.role,
          agentId,
        );

        // Initialize agents array if not present.
        if (!(parentTask as any).agents) {
          (parentTask as any).agents = [];
        }
        (parentTask as any).agents.push({
          id: agentId,
          type: payload.agentType,
          role: payload.role,
          status: "running",
          childTaskId: childTask.id,
        });

        // Wire event listener with agent ID tagging
        adapter.onEvent((event: any) => {
          const taggedEvent = { ...event, agentId, agentRole: payload.role };
          const now = Date.now();
          parentTask.lastActivityAt = now;
          childTask.lastActivityAt = now;
          parentTask.events.push(taggedEvent);
          childTask.events.push(taggedEvent);
          const win = getMainWindow();
          if (win && !win.isDestroyed()) {
            win.webContents.send("orchestrator:event", { taskId: parentTask.id, event: taggedEvent });
            win.webContents.send("orchestrator:event", { taskId: childTask.id, event: taggedEvent });
          }
        });

        adapter.onExit(async () => {
          const agents: any[] = (parentTask as any).agents ?? [];
          const agent = agents.find((a: any) => a.id === agentId);
          if (agent) agent.status = "done";
          childTask.status = "reviewing";
          childTask.completedAt = Date.now();
          childTask.lastActivityAt = childTask.completedAt;
          parentTask.lastActivityAt = childTask.completedAt;
          await orchestrator.persistTasks();
          const win = getMainWindow();
          if (win && !win.isDestroyed()) {
            win.webContents.send("orchestrator:task-status", { taskId: childTask.id, status: childTask.status });
          }
        });

        // Spawn in the same worktree
        await adapter.spawn(prompt, parentTask.worktreePath);

        await orchestrator.persistTasks();
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
        return { success: true, cost: cost ?? null };
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

        // Build effective prompt with plan mode prefix if enabled.
        let effectivePrompt = payload.prompt;
        if (resolved.planMode) {
          effectivePrompt =
            "First, propose an implementation plan. List files to change, approach, and risks. Wait for approval before making changes.\n\nTask: " +
            payload.prompt;
        }

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

        await orchestrator.startTask(task.id);

        if (resolved.planMode) {
          const t = orchestrator.getTask(task.id);
          if (t) {
            (t as any).status = "planning";
            const win = getMainWindow();
            if (win && !win.isDestroyed()) {
              win.webContents.send("orchestrator:task-status", { taskId: task.id, status: "planning" });
            }
          }
        }

        // Apply budget limit from template if set.
        if (resolved.budgetLimit) {
          costTracker.setBudget(task.id, resolved.budgetLimit);
          costTracker.persist();
        }

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

  const chatDb = new ChatDatabase();

  ipcMain.handle(
    "orchestrator:save-chat",
    async (_evt, payload: { taskId: string; messages: Array<{ id: string; role: string; kind?: string; content: string; events?: unknown[]; timestamp: number }> }) => {
      try {
        // Record that the renderer is actively saving for this task so
        // the main-process fallback persistence doesn't create duplicates.
        lastRendererSave.set(payload.taskId, Date.now());

        const rows = payload.messages.map((msg) => ({
          id: msg.id,
          task_id: payload.taskId,
          role: msg.role as "user" | "agent",
          kind: msg.kind ?? null,
          content: msg.content,
          events: msg.events ? JSON.stringify(msg.events) : null,
          timestamp: msg.timestamp,
        }));
        chatDb.saveMessages(payload.taskId, rows);
        return { success: true };
      } catch (err) {
        return { success: false, error: err instanceof Error ? err.message : String(err) };
      }
    },
  );

  ipcMain.handle(
    "orchestrator:load-chat",
    async (_evt, payload: { taskId: string }) => {
      try {
        const rows = chatDb.loadMessages(payload.taskId);
        const messages = dedupeLoadedChatMessages(rows.map((row) => ({
          id: row.id,
          role: row.role,
          kind: row.kind ?? undefined,
          content: row.content,
          events: row.events ? JSON.parse(row.events) : undefined,
          timestamp: row.timestamp,
        })));
        const task = orchestrator.getTask(payload.taskId);
        return { success: true, messages: ensureInitialPromptMessage(messages, task) };
      } catch (err) {
        return { success: false, error: err instanceof Error ? err.message : String(err) };
      }
    },
  );

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
      if ((task as any).status === "archived") continue;
      const prNumber = (task as any).prNumber;
      if (!prNumber) continue;

      // Check if PR is merged and enough time has passed
      if (task.completedAt && now - task.completedAt > autoArchiveDelay) {
        try {
          const { getPRChecks } = await import("./orchestrator-pr.js");
          const result = await getPRChecks(task.repoPath, prNumber);
          if (result.success && result.data?.pr?.state === "MERGED") {
            // Auto-archive
            (task as any).status = "archived";
            await orchestrator.persistTasks();
            const win = getMainWindow();
            if (win && !win.isDestroyed()) {
              win.webContents.send("orchestrator:task-status", { taskId: task.id, status: "archived" });
            }
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

  ipcMain.handle("orchestrator:rollback-thread", async (_evt, payload: { taskId: string; n: number }) => {
    try {
      const result = await orchestrator.rollbackThread(payload.taskId, payload.n);
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

  ipcMain.handle("orchestrator:set-goal", async (_evt, payload: { taskId: string; goal: string }) => {
    try {
      await orchestrator.setGoal(payload.taskId, payload.goal);
      return { success: true };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { success: false, error: message };
    }
  });

  ipcMain.handle("orchestrator:get-goal", async (_evt, payload: { taskId: string }) => {
    try {
      const result = await orchestrator.getGoal(payload.taskId);
      return { success: true, ...result };
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

  return orchestrator;
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/**
 * Builds a review-focused prompt for the AI SDK to review a diff.
 * The renderer calls `callAI` with this prompt to get inline review comments.
 */
function buildReviewPrompt(rawDiff: string, taskDescription: string): string {
  // Truncate very large diffs to avoid exceeding token limits.
  const maxDiffLength = 12_000;
  const truncatedDiff =
    rawDiff.length > maxDiffLength
      ? rawDiff.slice(0, maxDiffLength) + "\n\n[... diff truncated for review ...]"
      : rawDiff;

  return [
    "Review the following diff and provide concise, actionable feedback.",
    "Focus on: correctness, potential bugs, performance issues, security concerns, and code clarity.",
    "",
    "Format your response as markdown with sections:",
    "- **Summary**: One-sentence overall assessment",
    "- **Issues** (if any): List each issue with the file path, what's wrong, and how to fix it",
    "- **Suggestions** (if any): Non-critical improvements",
    "",
    "Be concise. If the code looks good, say so briefly.",
    "",
    `Task description: ${taskDescription}`,
    "",
    "Diff:",
    "```diff",
    truncatedDiff,
    "```",
  ].join("\n");
}
