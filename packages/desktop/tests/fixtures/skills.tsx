import { useEffect } from "react";
import { createRoot } from "react-dom/client";
import { SkillsView } from "../../src/views/SkillsView";
import { StoreProvider } from "../../src/state";
import {
  SettingsProvider,
  useSettings,
  type Settings,
} from "../../src/settings";
import { ApplicationDialogs } from "../../src/components/ApplicationDialogs";
import { ToastManager } from "../../src/components/ToastManager";
import type { SkillDetail, SkillOpResult } from "../../src/ipc";
import "../../src/index.css";

const params = new URLSearchParams(location.search);
const theme = (params.get("theme") ?? "light") as Settings["theme"];
let listFailure = params.has("list-error");
let createFailure = params.has("create-error");
let detailFailure = params.has("detail-error");
let scanFailure = params.has("scan-error");
const skills = new Map<string, SkillDetail>();
function makeSkill(
  name: string,
  description = "Reusable review instructions",
  body = "# Review\n\nRead **every change** and run tests.",
): SkillDetail {
  return {
    record: {
      id: name,
      name,
      description,
      scope: "user",
      repoPath: null,
      sourceUrl: null,
      installedAt: Date.now(),
      updatedAt: Date.now(),
      contentHash: "fixture",
      enabled: true,
      activationPrefs: { autoActivate: false, restrictToAgents: [] },
    },
    frontmatter: { name, description },
    body,
    agentStatus: { codex: "synced", gemini: "agent_not_installed" },
  };
}
if (params.has("populated"))
  for (const name of ["code-review", "write-docs"])
    skills.set(name, makeSkill(name));
const ok = <T,>(data: T): SkillOpResult<T> => ({ success: true, data });
if (!window.crab?.skills) {
  window.crab = {
    updateMainSettings: async () => ({}),
    setDevtools: async () => {},
    onEvent: () => () => {},
    rpc: async () => ({ exists: false, version: 0, jobs: [], error: null }),
    skills: {
      listSkills: async () => {
        if (listFailure) {
          listFailure = false;
          throw new Error("Library unavailable");
        }
        return ok(
          [...skills.values()].map((skill) => ({
            ...skill,
            bodyPreview: skill.body.slice(0, 100),
          })),
        );
      },
      getSkill: async (id) => {
        if (detailFailure) {
          detailFailure = false;
          return { success: false, error: "Skill file unavailable" };
        }
        const skill = skills.get(id)!;
        if (params.has("slow-detail") && id === "code-review")
          await new Promise((resolve) => setTimeout(resolve, 750));
        return ok(structuredClone(skill));
      },
      detectAgents: async () => {
        if (scanFailure) {
          scanFailure = false;
          throw new Error("Folder scan unavailable");
        }
        return [
          {
            id: "codex",
            name: "Codex CLI",
            configDir: "/fixture/.codex",
            installed: true,
          },
          {
            id: "gemini",
            name: "Gemini CLI",
            configDir: "/fixture/.gemini",
            installed: false,
          },
        ];
      },
      createSkill: async (data) => {
        if (createFailure) {
          createFailure = false;
          throw new Error("Disk write failed");
        }
        const skill = makeSkill(data.name, data.description, data.body);
        skills.set(data.name, skill);
        return ok(skill);
      },
      installFromUrl: async (url) => {
        const skill = makeSkill("installed-skill", url);
        skills.set(skill.record.id, skill);
        return ok(skill);
      },
      updateSkill: async ({ id, updates }) => {
        const skill = skills.get(id)!;
        Object.assign(skill.record, updates);
        Object.assign(skill.frontmatter, updates);
        if (updates.body !== undefined) skill.body = updates.body;
        return ok(skill);
      },
      deleteSkill: async (id) => {
        skills.delete(id);
        return ok(undefined);
      },
      toggleEnabled: async (id, enabled) => {
        skills.get(id)!.record.enabled = enabled;
        return ok(undefined);
      },
      resyncAll: async () => {
        if (params.has("sync-error")) throw new Error("Sync unavailable");
        if (params.has("sync-conflict"))
          skills.get("code-review")!.agentStatus.codex = "error";
        return ok({ synced: skills.size, skipped: 0, errors: 0 });
      },
    },
  } as typeof window.crab;
}
function Fixture() {
  const { update } = useSettings();
  useEffect(() => {
    update("theme", theme);
  }, [update]);
  return (
    <main className="h-screen bg-background">
      <SkillsView />
      <ApplicationDialogs />
      <ToastManager />
    </main>
  );
}
createRoot(document.getElementById("root")!).render(
  <StoreProvider>
    <SettingsProvider>
      <Fixture />
    </SettingsProvider>
  </StoreProvider>,
);
