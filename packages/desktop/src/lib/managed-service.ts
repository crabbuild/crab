import type { ManagedRepository } from "../ipc";

export interface ManagedRepositoryAccess {
  canClone: boolean;
  label: string;
  detail: string;
  tone: "success" | "warn" | "muted" | "error";
}

export interface ManagedErrorDetails {
  message: string;
  remediation: string | null;
}

export function managedRepositoryAccess(
  repository: ManagedRepository,
): ManagedRepositoryAccess {
  switch (repository.state) {
    case "active":
      return {
        canClone: true,
        label: repository.protected_push ? "Clone + protected push" : "Clone ready",
        detail: repository.protected_push
          ? "The service supports protected pushes after checkout."
          : "The repository is active and ready to clone.",
        tone: "success",
      };
    case "provisioning":
      return {
        canClone: false,
        label: "Provisioning",
        detail: "Wait for setup to finish, then refresh the catalog.",
        tone: "warn",
      };
    case "archived":
      return {
        canClone: false,
        label: "Archived",
        detail: "Ask an organization administrator to restore this repository.",
        tone: "muted",
      };
    case "soft_deleted":
      return {
        canClone: false,
        label: "Deleted",
        detail: "Ask an organization administrator whether this repository can be restored.",
        tone: "muted",
      };
    case "failed":
      return {
        canClone: false,
        label: "Setup failed",
        detail: "Ask the service operator to repair repository provisioning.",
        tone: "error",
      };
  }
}

export function managedErrorDetails(error: unknown): ManagedErrorDetails {
  if (!error || typeof error !== "object") {
    return { message: String(error), remediation: null };
  }
  const message = (error as { message?: unknown }).message;
  const data = (error as { data?: { remediation?: unknown } }).data;
  return {
    message: typeof message === "string" ? message : "Managed service request failed.",
    remediation: typeof data?.remediation === "string" ? data.remediation : null,
  };
}
