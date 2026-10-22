import type { OrganizationPage } from "../model";
import type { ActorProfile } from "@/features/profile/model";
export type Visibility = "closed" | "secret";
export type Role = "member" | "maintainer";
export interface Team {
  id: string;
  name: string;
  description: string;
  visibility: Visibility;
  version: number;
  created_at_ms: number;
  updated_at_ms: number;
}
export interface View {
  team: Team;
  role: Role | null;
  can_manage: boolean;
}
export type Listing = OrganizationPage<View & { members: number }> & {
  can_create: boolean;
};
export interface Member {
  actor: string;
  role: Role | null;
  version: number;
  profile: Pick<ActorProfile, "id" | "handle" | "name"> | null;
}
export type Roster = OrganizationPage<Member> & { can_manage: boolean };
export interface Policy {
  member_creation: boolean;
  version: number;
}
export type Draft = Pick<
  Team,
  "name" | "description" | "visibility" | "version"
>;
export type Command =
  | ({ kind: "create" } & Omit<Draft, "version">)
  | ({ kind: "edit"; team: string } & Draft)
  | { kind: "delete"; team: string; version: number }
  | {
      kind: "member";
      team: string;
      member: string;
      version: number;
      role: Role | null;
    }
  | ({ kind: "policy" } & Policy);
export interface Submission {
  request_id: string;
  command: Command;
}
export interface Acknowledgement {
  outcome: "applied" | "cancelled";
  id: string;
}
export function teamHref(organization: string, team?: string) {
  const query = new URLSearchParams({ tab: "teams" });
  if (team) query.set("team", team);
  return `/-/organizations/${organization}?${query}`;
}
