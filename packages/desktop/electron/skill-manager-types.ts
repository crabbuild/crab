/**
 * Shared types for the Skill Manager.
 */

export type SkillScope = "user" | "repo";

export interface SkillActivationPrefs {
  autoActivate: boolean;
  restrictToAgents: string[];
}

export interface SkillFrontmatter {
  name: string;
  description: string;
  license?: string;
  compatibility?: string;
  metadata?: Record<string, string>;
  'allowed-tools'?: string;
}

export interface SkillRecord {
  id: string;
  name: string;
  description: string;
  scope: SkillScope;
  repoPath: string | null;
  sourceUrl: string | null;
  installedAt: number;
  updatedAt: number;
  contentHash: string;
  enabled: boolean;
  activationPrefs: SkillActivationPrefs;
}

export interface DetectedAgent {
  id: string;
  name: string;
  configDir: string;
  installed: boolean;
}

export interface AgentSyncTarget {
  id: string;
  name: string;
  configDir: string;
  getSkillDir: (skillId: string) => string;
}

export interface SkillListItem {
  record: SkillRecord;
  frontmatter: SkillFrontmatter;
  bodyPreview: string;
  agentStatus: Record<string, "synced" | "agent_not_installed" | "error">;
}

export interface SkillDetail {
  record: SkillRecord;
  frontmatter: SkillFrontmatter;
  body: string;
  agentStatus: Record<string, "synced" | "agent_not_installed" | "error">;
}

export interface SkillOpResult<T = void> {
  success: boolean;
  data?: T;
  error?: string;
}
