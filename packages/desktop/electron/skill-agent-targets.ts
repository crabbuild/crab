import * as os from "node:os";
import * as path from "node:path";
import type { AgentSyncTarget } from "./skill-manager-types";

const home = os.homedir();

export const agentTargets: AgentSyncTarget[] = [
  {
    id: "claude-code",
    name: "Claude Code",
    configDir: path.join(home, ".claude"),
    getSkillDir: (skillId: string) =>
      path.join(home, ".claude", "commands", skillId),
  },
  {
    id: "codex",
    name: "Codex CLI",
    configDir: path.join(home, ".codex"),
    getSkillDir: (skillId: string) =>
      path.join(home, ".codex", "skills", skillId),
  },
  {
    id: "opencode",
    name: "OpenCode",
    configDir: path.join(home, ".config", "opencode"),
    getSkillDir: (skillId: string) =>
      path.join(home, ".config", "opencode", "skills", skillId),
  },
  {
    id: "cursor",
    name: "Cursor",
    configDir: path.join(home, ".cursor"),
    getSkillDir: (skillId: string) =>
      path.join(home, ".cursor", "skills", skillId),
  },
  {
    id: "gemini",
    name: "Gemini CLI",
    configDir: path.join(home, ".gemini"),
    getSkillDir: (skillId: string) =>
      path.join(home, ".gemini", "skills", skillId),
  },
  {
    id: "roo-code",
    name: "Roo Code",
    configDir: path.join(home, ".roo"),
    getSkillDir: (skillId: string) =>
      path.join(home, ".roo", "skills", skillId),
  },
  {
    id: "mistral-vibe",
    name: "Mistral Vibe",
    configDir: path.join(home, ".vibe"),
    getSkillDir: (skillId: string) =>
      path.join(home, ".vibe", "skills", skillId),
  },
];

export const skillScanPaths: string[] = [
  ...new Set(agentTargets.map((t) => path.dirname(t.getSkillDir("_")))),
  path.join(home, ".claude", "skills"),
  path.join(home, ".agent", "skills"),
  path.join(home, ".agents", "skills"),
];
