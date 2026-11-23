import type { RepositoryAccess } from "./contracts";

export const repositoryRoleLabels: Record<RepositoryAccess, string> = {
  read: "Read",
  triage: "Triage",
  moderate: "Moderate",
  write: "Write",
  maintain: "Maintain",
  admin: "Admin",
};

export const repositoryRoles = Object.keys(
  repositoryRoleLabels,
) as RepositoryAccess[];

export function isRepositoryRole(value: string): value is RepositoryAccess {
  return Object.hasOwn(repositoryRoleLabels, value);
}
