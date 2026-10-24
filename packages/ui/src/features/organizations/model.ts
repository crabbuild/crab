import type { ActorProfile } from "@/features/profile/model";
export type OrganizationRole = "member" | "owner";
export interface Organization {
  id: string;
  name: string;
  description: string;
  version: number;
  created_at_ms: number;
  updated_at_ms: number;
}
export interface Membership {
  actor: string;
  role: OrganizationRole;
  version: number;
  profile: Pick<ActorProfile, "id" | "handle" | "name"> | null;
}
export interface Invitation {
  recipient: string;
  role: OrganizationRole;
  version: number;
  state: "pending" | "accepted" | "declined" | "cancelled" | "expired";
  created_at_ms: number;
  expires_at_ms: number;
}
export interface InvitationView {
  organization: Organization | null;
  invitation: Invitation | null;
}
export interface OrganizationView {
  organization: Organization;
  actor: string;
  role: OrganizationRole;
}
export interface OrganizationPage<T> {
  items: T[];
  next: string | null;
  revision: number;
}
