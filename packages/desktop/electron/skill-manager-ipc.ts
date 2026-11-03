/**
 * Skill Manager IPC handlers — bridges main-process SkillManager to the renderer.
 */

import type { IpcMain } from "electron";
import { SkillManager } from "./skill-manager";
import type { SkillScope } from "./skill-manager-types";

export function registerSkillManagerIpc(
  ipcMain: IpcMain,
  skillManager: SkillManager,
  _getMainWindow: () => unknown,
): void {
  ipcMain.handle(
    "skills:list",
    async (_event, opts?: { scope?: SkillScope; repoPath?: string }) => {
      return skillManager.listSkills(opts);
    },
  );

  ipcMain.handle(
    "skills:get",
    async (_event, id: string) => {
      return skillManager.getSkill(id);
    },
  );

  ipcMain.handle(
    "skills:create",
    async (
      _event,
      data: {
        name: string;
        description: string;
        body?: string;
        scope: SkillScope;
        repoPath?: string;
      },
    ) => {
      return skillManager.createSkill(
        data.name,
        data.description,
        data.body,
        data.scope,
        data.repoPath,
      );
    },
  );

  ipcMain.handle(
    "skills:update",
    async (
      _event,
      data: {
        id: string;
        updates: { name?: string; description?: string; body?: string };
      },
    ) => {
      return skillManager.updateSkill(data.id, data.updates);
    },
  );

  ipcMain.handle(
    "skills:delete",
    async (_event, id: string) => {
      return skillManager.deleteSkill(id);
    },
  );

  ipcMain.handle(
    "skills:install-from-url",
    async (_event, url: string, scope: SkillScope, repoPath?: string) => {
      return skillManager.installSkillFromUrl(url, scope, repoPath);
    },
  );

  ipcMain.handle(
    "skills:toggle-enabled",
    async (_event, id: string, enabled: boolean) => {
      return skillManager.toggleEnabled(id, enabled);
    },
  );

  ipcMain.handle(
    "skills:set-activation-prefs",
    async (_event, id: string, prefs: unknown) => {
      return skillManager.setActivationPrefs(id, prefs as any);
    },
  );

  ipcMain.handle("skills:detect-agents", async () => {
    return skillManager.getDetectedAgents();
  });

  ipcMain.handle("skills:resync-all", async () => {
    return skillManager.resyncAll();
  });
}
