/**
 * Secure credential storage using the OS keychain.
 *
 * Provides a `keytar`-style interface (getPassword / setPassword / deletePassword)
 * for storing forge PATs and OAuth tokens in the OS-native credential store:
 *
 *   macOS:   Keychain Services (via `security` CLI)
 *   Linux:   libsecret / Secret Service (via `secret-tool`)
 *   Windows: DPAPI / Credential Manager (via `cmdkey`)
 *
 * We avoid the `keytar` npm package (deprecated, native module headaches)
 * and instead shell out to the platform CLI tools. This keeps the
 * dependency tree clean and works in all Electron packaging scenarios.
 *
 * The agent and renderer request tokens through IPC — keychain access
 * is main-process-only for security.
 */

import { execFile } from "child_process";
import { promisify } from "util";

const exec = promisify(execFile);

// ─── keytar-style public API ────────────────────────────────────────

/**
 * Retrieve a stored credential from the OS keychain.
 *
 * Returns `null` if the entry does not exist rather than throwing.
 */
export async function getPassword(service: string, account: string): Promise<string | null> {
  if (process.platform === "darwin") {
    return macosGet(service, account);
  } else if (process.platform === "linux") {
    return linuxGet(service, account);
  } else if (process.platform === "win32") {
    return windowsGet(service, account);
  }
  return null;
}

/**
 * Store a credential in the OS keychain.
 *
 * Creates or updates the entry for the given service+account pair.
 */
export async function setPassword(service: string, account: string, password: string): Promise<void> {
  if (process.platform === "darwin") {
    await macosSet(service, account, password);
  } else if (process.platform === "linux") {
    await linuxSet(service, account, password);
  } else if (process.platform === "win32") {
    await windowsSet(service, account, password);
  }
}

/**
 * Remove a credential from the OS keychain.
 *
 * Returns `true` if the entry was found and deleted, `false` if it
 * did not exist (never throws for missing entries).
 */
export async function deletePassword(service: string, account: string): Promise<boolean> {
  if (process.platform === "darwin") {
    return macosDelete(service, account);
  } else if (process.platform === "linux") {
    return linuxDelete(service, account);
  } else if (process.platform === "win32") {
    return windowsDelete(service, account);
  }
  return false;
}

// ─── Convenience wrappers (simplified single-key API) ───────────────
// These use "crab-desktop" as the service and key as the account,
// preserving backward compatibility with existing callers.

const DEFAULT_SERVICE = "crab-desktop";

/**
 * Store a credential under the default "crab-desktop" service.
 */
export async function setCredential(key: string, value: string): Promise<void> {
  await setPassword(DEFAULT_SERVICE, key, value);
}

/**
 * Retrieve a credential from the default "crab-desktop" service.
 * Returns null if not found.
 */
export async function getCredential(key: string): Promise<string | null> {
  return getPassword(DEFAULT_SERVICE, key);
}

/**
 * Delete a credential from the default "crab-desktop" service.
 */
export async function deleteCredential(key: string): Promise<boolean> {
  return deletePassword(DEFAULT_SERVICE, key);
}

/**
 * Check if a credential exists in the default "crab-desktop" service.
 */
export async function hasCredential(key: string): Promise<boolean> {
  const value = await getCredential(key);
  return value !== null;
}

// ─── macOS: Keychain Services via `security` CLI ────────────────────

async function macosSet(service: string, account: string, password: string): Promise<void> {
  try {
    await exec("security", [
      "add-generic-password",
      "-U", // update if exists
      "-s", service,
      "-a", account,
      "-w", password,
    ]);
  } catch {
    // If -U fails on older macOS, try delete + add
    try {
      await exec("security", ["delete-generic-password", "-s", service, "-a", account]);
    } catch { /* entry may not exist — fine */ }
    await exec("security", [
      "add-generic-password",
      "-s", service,
      "-a", account,
      "-w", password,
    ]);
  }
}

async function macosGet(service: string, account: string): Promise<string | null> {
  try {
    const { stdout } = await exec("security", [
      "find-generic-password",
      "-s", service,
      "-a", account,
      "-w", // output password only
    ]);
    return stdout.trim() || null;
  } catch {
    return null;
  }
}

async function macosDelete(service: string, account: string): Promise<boolean> {
  try {
    await exec("security", [
      "delete-generic-password",
      "-s", service,
      "-a", account,
    ]);
    return true;
  } catch {
    // Entry did not exist
    return false;
  }
}

// ─── Linux: libsecret via `secret-tool` CLI ─────────────────────────

async function linuxSet(service: string, account: string, password: string): Promise<void> {
  await exec("secret-tool", [
    "store",
    "--label", `${service}: ${account}`,
    "service", service,
    "account", account,
  ], { input: password } as any);
}

async function linuxGet(service: string, account: string): Promise<string | null> {
  try {
    const { stdout } = await exec("secret-tool", [
      "lookup",
      "service", service,
      "account", account,
    ]);
    return stdout.trim() || null;
  } catch {
    return null;
  }
}

async function linuxDelete(service: string, account: string): Promise<boolean> {
  try {
    await exec("secret-tool", [
      "clear",
      "service", service,
      "account", account,
    ]);
    return true;
  } catch {
    return false;
  }
}

// ─── Windows: DPAPI / Credential Manager via `cmdkey` ───────────────

function windowsTarget(service: string, account: string): string {
  return `${service}/${account}`;
}

async function windowsSet(service: string, account: string, password: string): Promise<void> {
  const target = windowsTarget(service, account);
  await exec("cmdkey", [
    `/generic:${target}`,
    `/user:${account}`,
    `/pass:${password}`,
  ]);
}

async function windowsGet(service: string, account: string): Promise<string | null> {
  const target = windowsTarget(service, account);
  try {
    // Verify the credential exists via cmdkey /list first.
    await exec("cmdkey", [`/list:${target}`]);
    // cmdkey doesn't directly expose the password in plain text.
    // Use PowerShell's Get-StoredCredential for DPAPI retrieval.
    const { stdout: psOut } = await exec("powershell", [
      "-NoProfile",
      "-Command",
      `$cred = Get-StoredCredential -Target '${target}' -ErrorAction SilentlyContinue; if ($cred) { $cred.GetNetworkCredential().Password } else { '' }`,
    ]);
    const result = psOut.trim();
    return result || null;
  } catch {
    // Fallback: try reading via cmdkey list pattern match
    try {
      const { stdout: listOut } = await exec("cmdkey", ["/list"]);
      if (listOut.includes(target)) {
        // Entry exists but we can't read the password via cmdkey alone.
        // Return a sentinel or null — in practice the PowerShell path
        // above handles this. If PowerShell isn't available, return null.
        return null;
      }
    } catch { /* ignore */ }
    return null;
  }
}

async function windowsDelete(service: string, account: string): Promise<boolean> {
  const target = windowsTarget(service, account);
  try {
    await exec("cmdkey", [`/delete:${target}`]);
    return true;
  } catch {
    return false;
  }
}

// ─── IPC handler registration ───────────────────────────────────────

/**
 * Register IPC handlers for keychain operations.
 * Call this from the main process during app initialization.
 *
 * Exposes two sets of channels:
 * 1. Full keytar-style API (service + account):
 *    - crab:keychain-get-password
 *    - crab:keychain-set-password
 *    - crab:keychain-delete-password
 *
 * 2. Simplified API (single key, uses "crab-desktop" as service):
 *    - crab:keychain-set
 *    - crab:keychain-get
 *    - crab:keychain-delete
 *    - crab:keychain-has
 */
export function registerKeychainHandlers(ipcMain: Electron.IpcMain): void {
  // Full keytar-style API — used by forge auth and agent token requests
  ipcMain.handle(
    "crab:keychain-get-password",
    async (_event, service: string, account: string) => {
      return getPassword(service, account);
    },
  );

  ipcMain.handle(
    "crab:keychain-set-password",
    async (_event, service: string, account: string, password: string) => {
      await setPassword(service, account, password);
      return { ok: true };
    },
  );

  ipcMain.handle(
    "crab:keychain-delete-password",
    async (_event, service: string, account: string) => {
      return deletePassword(service, account);
    },
  );

  // Simplified API — backward-compatible single-key interface
  ipcMain.handle("crab:keychain-set", async (_event, key: string, value: string) => {
    await setCredential(key, value);
    return { ok: true };
  });

  ipcMain.handle("crab:keychain-get", async (_event, key: string) => {
    return getCredential(key);
  });

  ipcMain.handle("crab:keychain-delete", async (_event, key: string) => {
    return deleteCredential(key);
  });

  ipcMain.handle("crab:keychain-has", async (_event, key: string) => {
    return hasCredential(key);
  });
}
