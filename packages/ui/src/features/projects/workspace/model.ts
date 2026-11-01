import type { Repository } from "@/lib/api";
import type { Command, Item, Project } from "../model";

export type LinkedRepository = Pick<Repository, "id" | "owner" | "name">;
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
export interface Detail {
  project: WorkspaceProject;
  can_manage: boolean;
  repositories: LinkedRepository[];
  restricted_repositories: number;
  lead: { id: string; handle: string; name: string } | null;
}
export type WorkspaceCommand =
  | Extract<Command, { kind: "create" | "edit" | "remove" }>
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
