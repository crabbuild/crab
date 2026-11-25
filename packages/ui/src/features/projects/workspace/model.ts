import type { Repository } from "@/lib/api";
import type { IssueType, ProjectAccessRole } from "@/lib/contracts";
import type { Command, Item, Project } from "../model";

export type LinkedRepository = Pick<Repository, "id" | "owner" | "name"> & {
  can_create_issue?: boolean;
};
export const statuses = {
  planned: "Planned",
  in_progress: "In progress",
  paused: "Paused",
  completed: "Completed",
  cancelled: "Cancelled",
} as const;
export const health = {
  on_track: "On track",
  at_risk: "At risk",
  off_track: "Off track",
} as const;
export interface Properties {
  status: keyof typeof statuses;
  health: keyof typeof health;
  lead: string | null;
  start: string | null;
  target: string | null;
}
export interface WorkspaceProject extends Project {
  properties: Properties;
}
export interface WorkspaceListing {
  items: WorkspaceProject[];
  next: string | null;
  revision: number;
  can_create: boolean;
}
export type ProjectAssociation = { team: string } | { repository: string };
export interface Detail {
  project: WorkspaceProject;
  can_manage: boolean;
  can_admin?: boolean;
  repositories: LinkedRepository[];
  restricted_repositories: number;
  default_repository?: string | null;
  default_repository_configured?: boolean;
  issue_types?: IssueType[];
  lead: { id: string; handle: string; name: string } | null;
}
export type AccessRole = ProjectAccessRole;
export const projectAccessRoles = {
  none: "No access",
  read: "Read",
  write: "Write",
  admin: "Admin",
} satisfies Record<AccessRole, string>;
export type GrantTarget = { kind: "member" | "team" | "outside"; id: string };
export type AccessTarget = { kind: "base" } | GrantTarget;
export interface ProjectAccessView {
  version: number;
  base: AccessRole;
  base_override: AccessRole | null;
  default_role: AccessRole;
  default_version: number;
  can_manage_teams: boolean;
  grants: {
    target: GrantTarget;
    role: AccessRole;
    active: boolean;
    label: string | null;
  }[];
}
export type WorkspaceCommand =
  | Extract<
      Command,
      {
        kind:
          | "create"
          | "edit"
          | "column_limit"
          | "reorder_views"
          | "remove"
          | "bulk_fields";
      }
    >
  | {
      kind: "set_access";
      project: string;
      version: number;
      target: AccessTarget;
      role: AccessRole | null;
      default_version?: number;
    }
  | {
      kind: "link_team";
      project: string;
      version: number;
      team: string;
    }
  | {
      kind: "link_repository";
      project: string;
      version: number;
      repository: string;
    }
  | {
      kind: "set_default_repository";
      project: string;
      version: number;
      repository: string | null;
    }
  | {
      kind: "set_default_access";
      version: number;
      role: AccessRole;
      scope: string;
    }
  | {
      kind: "create_issue";
      project: string;
      repository: string;
      item: Item;
      type_id: string | null;
      draft_version?: number;
    }
  | {
      kind: "planning";
      project: string;
      version: number;
      properties: Properties;
    }
  | {
      kind: "repository";
      project: string;
      version: number;
      repository: string;
      linked: boolean;
    }
  | {
      kind: "item";
      project: string;
      version: number;
      repository: string | null;
      item: Item;
    };
export function workspaceHref(organization: string, project?: string) {
  return `/-/organizations/${organization}/projects${project ? `/${project}` : ""}`;
}
