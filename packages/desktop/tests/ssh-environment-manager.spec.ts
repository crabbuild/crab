/**
 * Tests for RemoteEnvironmentManager — environment variable and PATH
 * management on remote servers via the agent RPC interface.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  RemoteEnvironmentManager,
  type EnvListResult,
  type EnvSetResult,
  type EnvValidationError,
} from "../electron/ssh-environment-manager";
import type { RpcRouter } from "../electron/ssh-router";
import type { SshSessionManager } from "../electron/ssh-manager";

// ── Mock helpers ─────────────────────────────────────────────────

function createMockRouter(response: unknown = { success: true }) {
  return {
    sendToRemoteAgent: vi.fn().mockResolvedValue(response),
  } as unknown as RpcRouter;
}

function createMockSessionManager(status = "connected") {
  return {
    getStatus: vi.fn().mockReturnValue(status),
  } as unknown as SshSessionManager;
}

// ── Environment variable name validation ─────────────────────────

describe("RemoteEnvironmentManager — name validation", () => {
  let manager: RemoteEnvironmentManager;

  beforeEach(() => {
    manager = new RemoteEnvironmentManager(
      createMockRouter(),
      createMockSessionManager(),
    );
  });

  it("accepts valid names with letters and underscores", () => {
    expect(manager.validateName("MY_VAR")).toBeNull();
    expect(manager.validateName("_PRIVATE")).toBeNull();
    expect(manager.validateName("a")).toBeNull();
    expect(manager.validateName("PATH")).toBeNull();
    expect(manager.validateName("HOME")).toBeNull();
  });

  it("accepts names with alphanumeric characters (not leading digit)", () => {
    expect(manager.validateName("VAR1")).toBeNull();
    expect(manager.validateName("MY_VAR_2")).toBeNull();
    expect(manager.validateName("_123")).toBeNull();
    expect(manager.validateName("A1B2C3")).toBeNull();
  });

  it("rejects names starting with a digit", () => {
    const result = manager.validateName("1VAR");
    expect(result).not.toBeNull();
    expect(result!.code).toBe("INVALID_NAME");
    expect(result!.message).toContain("must not start with a digit");
  });

  it("rejects names with special characters", () => {
    expect(manager.validateName("MY-VAR")).not.toBeNull();
    expect(manager.validateName("MY.VAR")).not.toBeNull();
    expect(manager.validateName("MY VAR")).not.toBeNull();
    expect(manager.validateName("MY$VAR")).not.toBeNull();
    expect(manager.validateName("MY@VAR")).not.toBeNull();
  });

  it("rejects empty names", () => {
    const result = manager.validateName("");
    expect(result).not.toBeNull();
    expect(result!.code).toBe("NAME_TOO_SHORT");
  });

  it("rejects names exceeding 255 characters", () => {
    const longName = "A".repeat(256);
    const result = manager.validateName(longName);
    expect(result).not.toBeNull();
    expect(result!.code).toBe("NAME_TOO_LONG");
    expect(result!.message).toContain("256");
  });

  it("accepts names at exactly 255 characters", () => {
    const maxName = "A".repeat(255);
    expect(manager.validateName(maxName)).toBeNull();
  });

  it("accepts single character names", () => {
    expect(manager.validateName("A")).toBeNull();
    expect(manager.validateName("_")).toBeNull();
  });
});

// ── Environment variable value validation ────────────────────────

describe("RemoteEnvironmentManager — value validation", () => {
  let manager: RemoteEnvironmentManager;

  beforeEach(() => {
    manager = new RemoteEnvironmentManager(
      createMockRouter(),
      createMockSessionManager(),
    );
  });

  it("accepts values within the 32,768 character limit", () => {
    expect(manager.validateValue("hello")).toBeNull();
    expect(manager.validateValue("")).toBeNull();
    expect(manager.validateValue("A".repeat(32_768))).toBeNull();
  });

  it("rejects values exceeding 32,768 characters", () => {
    const longValue = "A".repeat(32_769);
    const result = manager.validateValue(longValue);
    expect(result).not.toBeNull();
    expect(result!.code).toBe("VALUE_TOO_LONG");
    expect(result!.message).toContain("32769");
  });
});

// ── listEnvironment ──────────────────────────────────────────────

describe("RemoteEnvironmentManager — listEnvironment", () => {
  it("returns environment variables, PATH entries, shell, and config files", async () => {
    const router = createMockRouter({
      variables: { HOME: "/home/dev", PATH: "/usr/bin:/bin", EDITOR: "vim" },
      path_entries: ["/usr/bin", "/bin", "/usr/local/bin"],
      shell: "bash",
      config_files: ["/home/dev/.bashrc", "/home/dev/.profile"],
    });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.listEnvironment("profile-1");

    expect(result.variables).toEqual({
      HOME: "/home/dev",
      PATH: "/usr/bin:/bin",
      EDITOR: "vim",
    });
    expect(result.pathEntries).toEqual(["/usr/bin", "/bin", "/usr/local/bin"]);
    expect(result.shell).toBe("bash");
    expect(result.configFiles).toEqual(["/home/dev/.bashrc", "/home/dev/.profile"]);
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("profile-1", "env_list", {});
  });

  it("limits environment variables to 1000 entries", async () => {
    const variables: Record<string, string> = {};
    for (let i = 0; i < 1100; i++) {
      variables[`VAR_${i}`] = `value_${i}`;
    }
    const router = createMockRouter({
      variables,
      path_entries: [],
      shell: "bash",
      config_files: [],
    });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.listEnvironment("profile-1");

    expect(Object.keys(result.variables).length).toBe(1000);
  });

  it("limits PATH entries to 200", async () => {
    const pathEntries = Array.from({ length: 250 }, (_, i) => `/path/${i}`);
    const router = createMockRouter({
      variables: {},
      path_entries: pathEntries,
      shell: "zsh",
      config_files: [],
    });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.listEnvironment("profile-1");

    expect(result.pathEntries.length).toBe(200);
  });

  it("throws when not connected", async () => {
    const manager = new RemoteEnvironmentManager(
      createMockRouter(),
      createMockSessionManager("disconnected"),
    );

    await expect(manager.listEnvironment("profile-1")).rejects.toThrow(
      /SSH connection status is "disconnected"/,
    );
  });

  it("handles missing fields gracefully", async () => {
    const router = createMockRouter({});
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.listEnvironment("profile-1");

    expect(result.variables).toEqual({});
    expect(result.pathEntries).toEqual([]);
    expect(result.shell).toBe("bash");
    expect(result.configFiles).toEqual([]);
  });
});

// ── setVariable ──────────────────────────────────────────────────

describe("RemoteEnvironmentManager — setVariable", () => {
  it("sends env_set RPC with correct params", async () => {
    const router = createMockRouter({ success: true });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.setVariable("profile-1", "MY_VAR", "hello", false);

    expect(result.success).toBe(true);
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("profile-1", "env_set", {
      name: "MY_VAR",
      value: "hello",
      persist: false,
    });
  });

  it("includes config_file when persist is true", async () => {
    const router = createMockRouter({ success: true, backup_path: "/home/dev/.bashrc.bak.1" });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.setVariable(
      "profile-1",
      "MY_VAR",
      "hello",
      true,
      "/home/dev/.bashrc",
    );

    expect(result.success).toBe(true);
    expect(result.backupPath).toBe("/home/dev/.bashrc.bak.1");
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("profile-1", "env_set", {
      name: "MY_VAR",
      value: "hello",
      persist: true,
      config_file: "/home/dev/.bashrc",
    });
  });

  it("returns syntax error when shell validation fails", async () => {
    const router = createMockRouter({
      success: false,
      backup_path: "/home/dev/.bashrc.bak.1",
      syntax_error: "line 42: unexpected EOF",
    });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.setVariable(
      "profile-1",
      "MY_VAR",
      "hello",
      true,
      "/home/dev/.bashrc",
    );

    expect(result.success).toBe(false);
    expect(result.syntaxError).toBe("line 42: unexpected EOF");
    expect(result.backupPath).toBe("/home/dev/.bashrc.bak.1");
  });

  it("emits env-syntax-error event on syntax failure", async () => {
    const router = createMockRouter({
      success: false,
      backup_path: "/home/dev/.bashrc.bak.1",
      syntax_error: "line 42: unexpected EOF",
    });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());
    const handler = vi.fn();
    manager.on("env-syntax-error", handler);

    await manager.setVariable("profile-1", "MY_VAR", "hello", true, "/home/dev/.bashrc");

    expect(handler).toHaveBeenCalledWith({
      profileId: "profile-1",
      configFile: "/home/dev/.bashrc",
      error: "line 42: unexpected EOF",
      backupPath: "/home/dev/.bashrc.bak.1",
    });
  });

  it("throws on invalid variable name", async () => {
    const manager = new RemoteEnvironmentManager(
      createMockRouter(),
      createMockSessionManager(),
    );

    await expect(
      manager.setVariable("profile-1", "1INVALID", "value", false),
    ).rejects.toThrow(/must not start with a digit/);
  });

  it("throws on value exceeding max length", async () => {
    const manager = new RemoteEnvironmentManager(
      createMockRouter(),
      createMockSessionManager(),
    );

    await expect(
      manager.setVariable("profile-1", "MY_VAR", "A".repeat(32_769), false),
    ).rejects.toThrow(/must not exceed 32768 characters/);
  });

  it("throws when not connected", async () => {
    const manager = new RemoteEnvironmentManager(
      createMockRouter(),
      createMockSessionManager("disconnected"),
    );

    await expect(
      manager.setVariable("profile-1", "MY_VAR", "value", false),
    ).rejects.toThrow(/SSH connection status is "disconnected"/);
  });

  it("emits env-updated on success", async () => {
    const router = createMockRouter({ success: true });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());
    const handler = vi.fn();
    manager.on("env-updated", handler);

    await manager.setVariable("profile-1", "MY_VAR", "hello", false);

    expect(handler).toHaveBeenCalledWith({ profileId: "profile-1" });
  });
});

// ── deleteVariable ───────────────────────────────────────────────

describe("RemoteEnvironmentManager — deleteVariable", () => {
  it("sends env_delete RPC with correct params", async () => {
    const router = createMockRouter({ success: true });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    await manager.deleteVariable("profile-1", "MY_VAR");

    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("profile-1", "env_delete", {
      name: "MY_VAR",
    });
  });

  it("throws on invalid variable name", async () => {
    const manager = new RemoteEnvironmentManager(
      createMockRouter(),
      createMockSessionManager(),
    );

    await expect(
      manager.deleteVariable("profile-1", "INVALID-NAME"),
    ).rejects.toThrow(/alphanumeric characters and underscores/);
  });

  it("emits env-updated on success", async () => {
    const router = createMockRouter({ success: true });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());
    const handler = vi.fn();
    manager.on("env-updated", handler);

    await manager.deleteVariable("profile-1", "MY_VAR");

    expect(handler).toHaveBeenCalledWith({ profileId: "profile-1" });
  });

  it("throws when not connected", async () => {
    const manager = new RemoteEnvironmentManager(
      createMockRouter(),
      createMockSessionManager("reconnecting"),
    );

    await expect(
      manager.deleteVariable("profile-1", "MY_VAR"),
    ).rejects.toThrow(/SSH connection status is "reconnecting"/);
  });
});

// ── listPath ─────────────────────────────────────────────────────

describe("RemoteEnvironmentManager — listPath", () => {
  it("returns PATH entries from the agent", async () => {
    const router = createMockRouter({
      entries: ["/usr/bin", "/bin", "/usr/local/bin"],
    });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.listPath("profile-1");

    expect(result).toEqual(["/usr/bin", "/bin", "/usr/local/bin"]);
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("profile-1", "path_list", {});
  });

  it("limits entries to 200", async () => {
    const entries = Array.from({ length: 250 }, (_, i) => `/path/${i}`);
    const router = createMockRouter({ entries });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.listPath("profile-1");

    expect(result.length).toBe(200);
  });

  it("returns empty array when no entries", async () => {
    const router = createMockRouter({});
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.listPath("profile-1");

    expect(result).toEqual([]);
  });
});

// ── updatePath ───────────────────────────────────────────────────

describe("RemoteEnvironmentManager — updatePath", () => {
  it("sends path_update RPC with entries", async () => {
    const router = createMockRouter({ success: true });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.updatePath("profile-1", ["/usr/bin", "/bin"]);

    expect(result.success).toBe(true);
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("profile-1", "path_update", {
      entries: ["/usr/bin", "/bin"],
    });
  });

  it("includes persist and config_file when specified", async () => {
    const router = createMockRouter({ success: true, backup_path: "/home/dev/.bashrc.bak.1" });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.updatePath(
      "profile-1",
      ["/usr/bin", "/bin"],
      true,
      "/home/dev/.bashrc",
    );

    expect(result.success).toBe(true);
    expect(result.backupPath).toBe("/home/dev/.bashrc.bak.1");
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("profile-1", "path_update", {
      entries: ["/usr/bin", "/bin"],
      persist: true,
      config_file: "/home/dev/.bashrc",
    });
  });

  it("returns syntax error when shell validation fails", async () => {
    const router = createMockRouter({
      success: false,
      backup_path: "/home/dev/.zshrc.bak.1",
      syntax_error: "parse error near `\\n'",
    });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.updatePath(
      "profile-1",
      ["/usr/bin"],
      true,
      "/home/dev/.zshrc",
    );

    expect(result.success).toBe(false);
    expect(result.syntaxError).toBe("parse error near `\\n'");
    expect(result.backupPath).toBe("/home/dev/.zshrc.bak.1");
  });

  it("throws when entries exceed 200", async () => {
    const manager = new RemoteEnvironmentManager(
      createMockRouter(),
      createMockSessionManager(),
    );
    const entries = Array.from({ length: 201 }, (_, i) => `/path/${i}`);

    await expect(
      manager.updatePath("profile-1", entries),
    ).rejects.toThrow(/PATH entries exceed maximum of 200/);
  });

  it("emits env-updated on success", async () => {
    const router = createMockRouter({ success: true });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());
    const handler = vi.fn();
    manager.on("env-updated", handler);

    await manager.updatePath("profile-1", ["/usr/bin"]);

    expect(handler).toHaveBeenCalledWith({ profileId: "profile-1" });
  });
});

// ── revertConfig ─────────────────────────────────────────────────

describe("RemoteEnvironmentManager — revertConfig", () => {
  it("sends env_revert RPC with backup and config paths", async () => {
    const router = createMockRouter({ success: true });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.revertConfig(
      "profile-1",
      "/home/dev/.bashrc.bak.1",
      "/home/dev/.bashrc",
    );

    expect(result).toBe(true);
    expect(router.sendToRemoteAgent).toHaveBeenCalledWith("profile-1", "env_revert", {
      backup_path: "/home/dev/.bashrc.bak.1",
      config_file: "/home/dev/.bashrc",
    });
  });

  it("returns false when revert fails", async () => {
    const router = createMockRouter({ success: false });
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());

    const result = await manager.revertConfig(
      "profile-1",
      "/home/dev/.bashrc.bak.1",
      "/home/dev/.bashrc",
    );

    expect(result).toBe(false);
  });
});

// ── Error handling ───────────────────────────────────────────────

describe("RemoteEnvironmentManager — error handling", () => {
  it("emits error event when RPC call fails", async () => {
    const router = {
      sendToRemoteAgent: vi.fn().mockRejectedValue(new Error("RPC timeout")),
    } as unknown as RpcRouter;
    const manager = new RemoteEnvironmentManager(router, createMockSessionManager());
    const handler = vi.fn();
    manager.on("error", handler);

    await expect(manager.listEnvironment("profile-1")).rejects.toThrow("RPC timeout");

    expect(handler).toHaveBeenCalledWith({
      profileId: "profile-1",
      error: "RPC timeout",
    });
  });
});
