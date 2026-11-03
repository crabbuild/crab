/**
 * Enhanced connection hooks — performs capability negotiation and starts
 * quality monitoring after an SSH connection is established.
 *
 * Call `setupEnhancedConnectionHooks(modules)` once during app initialization.
 * It listens for the session manager's "ssh:status-changed" event and, when
 * a profile transitions to "connected", runs the post-connect sequence:
 *
 * 1. Negotiate agent capabilities via CapabilityNegotiator
 * 2. Start ConnectionQualityMonitor for the profile
 * 3. Emit capabilities to the renderer so the UI can enable/disable features
 *
 * On disconnect, it cleans up monitoring and clears cached capabilities.
 */

import { BrowserWindow } from "electron";
import type { SshSessionManager } from "./ssh-manager";
import type { CapabilityNegotiator } from "./ssh-capabilities";
import type { ConnectionQualityMonitor } from "./ssh-connection-quality";
import type { AgentCapabilities } from "./ssh-types";

// ── Types ────────────────────────────────────────────────────────

export interface ConnectionHookModules {
  sessionManager: SshSessionManager;
  capabilityNegotiator: CapabilityNegotiator;
  connectionQualityMonitor: ConnectionQualityMonitor;
}

// ── Setup ────────────────────────────────────────────────────────

/**
 * Wire post-connection hooks that negotiate capabilities and start
 * quality monitoring whenever an SSH session reaches "connected" state.
 *
 * Also handles cleanup on disconnect: stops monitoring and clears
 * the capability cache for the disconnected profile.
 */
export function setupEnhancedConnectionHooks(modules: ConnectionHookModules): void {
  const { sessionManager } = modules;

  sessionManager.on(
    "ssh:status-changed",
    (payload: { profileId: string; status: string; error?: string }) => {
      if (payload.status === "connected") {
        void onConnected(payload.profileId, modules);
      } else if (payload.status === "disconnected" || payload.status === "error") {
        onDisconnected(payload.profileId, modules);
      }
    },
  );
}

// ── Internal ─────────────────────────────────────────────────────

/**
 * Post-connection sequence:
 * 1. Negotiate capabilities with the remote agent
 * 2. Start connection quality monitoring
 * 3. Broadcast capabilities to the renderer
 */
async function onConnected(profileId: string, modules: ConnectionHookModules): Promise<void> {
  const { capabilityNegotiator, connectionQualityMonitor } = modules;

  // Step 1: Negotiate capabilities
  let capabilities: AgentCapabilities | null = null;
  try {
    capabilities = await capabilityNegotiator.negotiate(profileId);
  } catch {
    // If negotiation fails (e.g., old agent without capabilities RPC),
    // fall back to a minimal capability set — all features disabled.
    capabilities = {
      version: "0.0.0",
      features: {
        fileWatching: false,
        deltaSync: false,
        compression: false,
        processManagement: false,
        environmentManagement: false,
        debugging: false,
        search: false,
        buildRun: false,
        logTailing: false,
      },
    };
  }

  // Step 2: Start connection quality monitoring
  connectionQualityMonitor.startMonitoring(profileId);

  // Step 3: Emit capabilities to the renderer so UI can gate features
  broadcastCapabilities(profileId, capabilities);
}

/**
 * Cleanup on disconnect: stop quality monitoring and clear cached
 * capabilities so the next connect triggers fresh negotiation.
 */
function onDisconnected(profileId: string, modules: ConnectionHookModules): void {
  const { capabilityNegotiator, connectionQualityMonitor } = modules;

  connectionQualityMonitor.stopMonitoring(profileId);
  capabilityNegotiator.clearCache(profileId);
}

/**
 * Broadcast the negotiated capabilities to all renderer windows.
 * The renderer uses this to enable/disable UI features based on
 * what the remote agent supports.
 */
function broadcastCapabilities(profileId: string, capabilities: AgentCapabilities): void {
  const windows = BrowserWindow.getAllWindows();
  for (const win of windows) {
    win.webContents.send("ssh:capabilities", { profileId, capabilities });
  }
}
