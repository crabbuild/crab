import { describe, expect, it } from "vitest";
import type { ManagedRepository } from "../src/ipc";
import {
  managedErrorDetails,
  managedRepositoryAccess,
} from "../src/lib/managed-service";

function repository(
  state: ManagedRepository["state"],
  protected_push = false,
): ManagedRepository {
  return {
    schema_version: 1,
    repository_id: "00000000-0000-0000-0000-000000000001",
    organization_id: "00000000-0000-0000-0000-000000000002",
    canonical_url: "crab://crab.build/acme/models",
    state,
    revision: 1,
    transfer_modes: ["direct"],
    protected_push,
  };
}

describe("managed repository access", () => {
  it("allows cloning an active repository", () => {
    expect(managedRepositoryAccess(repository("active"))).toMatchObject({
      canClone: true,
      label: "Clone ready",
    });
  });

  it("shows protected push without changing the canonical clone URL", () => {
    const managed = repository("active", true);
    expect(managedRepositoryAccess(managed).label).toBe("Clone + protected push");
    expect(managed.canonical_url).toBe("crab://crab.build/acme/models");
  });

  it.each(["provisioning", "archived", "soft_deleted", "failed"] as const)(
    "blocks cloning while the repository is %s",
    (state) => {
      expect(managedRepositoryAccess(repository(state)).canClone).toBe(false);
    },
  );
});

describe("managed errors", () => {
  it("surfaces the agent remediation separately from the safe message", () => {
    expect(managedErrorDetails({
      message: "managed repository access is forbidden",
      data: { remediation: "Ask an organization owner for access.", secret: "hidden" },
    })).toEqual({
      message: "managed repository access is forbidden",
      remediation: "Ask an organization owner for access.",
    });
  });
});
