/**
 * SSH Command Palette Integration — registers SSH commands in the
 * application command palette and provides quick-connect functionality.
 *
 * Commands:
 * - SSH: Connect to...
 * - SSH: Disconnect from...
 * - SSH: Open Terminal on...
 * - SSH: Browse Files on...
 * - SSH: Add Connection
 * - SSH: Import from SSH Config
 * - SSH: Run Diagnostics on...
 */

// Command palette integration — no React component needed here

// ── Types ────────────────────────────────────────────────────────

export interface SshCommand {
  id: string;
  label: string;
  category: string;
  shortcut?: string;
  icon?: string;
  action: () => void;
  when?: () => boolean;
}

export interface SshProfile {
  id: string;
  label: string;
  host: string;
  status: "connected" | "disconnected" | "connecting" | "reconnecting";
}

interface SshCommandPaletteOptions {
  profiles: SshProfile[];
  onConnect: (profileId: string) => void;
  onDisconnect: (profileId: string) => void;
  onOpenTerminal: (profileId: string) => void;
  onBrowseFiles: (profileId: string) => void;
  onAddConnection: () => void;
  onImportConfig: () => void;
  onRunDiagnostics: (profileId: string) => void;
  onOpenSshView: () => void;
}

// ── Command Registration ─────────────────────────────────────────

/**
 * Generate SSH commands for the command palette. These are registered
 * with the application's command registry.
 */
export function registerSshCommands(options: SshCommandPaletteOptions): SshCommand[] {
  const {
    profiles,
    onConnect,
    onDisconnect,
    onOpenTerminal,
    onBrowseFiles,
    onAddConnection,
    onImportConfig,
    onRunDiagnostics,
    onOpenSshView,
  } = options;

  const commands: SshCommand[] = [];

  // Static commands (always available)
  commands.push({
    id: "ssh.add-connection",
    label: "SSH: Add Connection",
    category: "SSH",
    icon: "➕",
    action: onAddConnection,
  });

  commands.push({
    id: "ssh.import-config",
    label: "SSH: Import from SSH Config",
    category: "SSH",
    icon: "📥",
    action: onImportConfig,
  });

  commands.push({
    id: "ssh.open-view",
    label: "SSH: Open SSH View",
    category: "SSH",
    shortcut: "Cmd+Shift+S",
    icon: "🖥️",
    action: onOpenSshView,
  });

  // Dynamic commands based on profiles
  const disconnectedProfiles = profiles.filter((p) => p.status === "disconnected");
  const connectedProfiles = profiles.filter((p) => p.status === "connected");

  // Connect commands
  for (const profile of disconnectedProfiles) {
    commands.push({
      id: `ssh.connect.${profile.id}`,
      label: `SSH: Connect to ${profile.label}`,
      category: "SSH",
      icon: "🔌",
      action: () => onConnect(profile.id),
    });
  }

  // Disconnect commands
  for (const profile of connectedProfiles) {
    commands.push({
      id: `ssh.disconnect.${profile.id}`,
      label: `SSH: Disconnect from ${profile.label}`,
      category: "SSH",
      icon: "⏏️",
      action: () => onDisconnect(profile.id),
    });
  }

  // Terminal commands (connected only)
  for (const profile of connectedProfiles) {
    commands.push({
      id: `ssh.terminal.${profile.id}`,
      label: `SSH: Open Terminal on ${profile.label}`,
      category: "SSH",
      shortcut: "Cmd+Shift+T",
      icon: "⬛",
      action: () => onOpenTerminal(profile.id),
    });
  }

  // Browse commands (connected only)
  for (const profile of connectedProfiles) {
    commands.push({
      id: `ssh.browse.${profile.id}`,
      label: `SSH: Browse Files on ${profile.label}`,
      category: "SSH",
      icon: "📁",
      action: () => onBrowseFiles(profile.id),
    });
  }

  // Diagnostics commands (connected only)
  for (const profile of connectedProfiles) {
    commands.push({
      id: `ssh.diagnostics.${profile.id}`,
      label: `SSH: Run Diagnostics on ${profile.label}`,
      category: "SSH",
      icon: "🔍",
      action: () => onRunDiagnostics(profile.id),
    });
  }

  return commands;
}

/**
 * Fuzzy search across SSH commands and profile names.
 * Returns commands matching the query, sorted by relevance.
 */
export function searchSshCommands(commands: SshCommand[], query: string): SshCommand[] {
  if (!query.trim()) return commands;

  const lower = query.toLowerCase();
  const scored = commands
    .map((cmd) => {
      const label = cmd.label.toLowerCase();
      let score = 0;

      // Exact prefix match
      if (label.startsWith(lower)) score += 100;
      // Contains match
      else if (label.includes(lower)) score += 50;
      // Fuzzy match (all chars in order)
      else {
        let qi = 0;
        for (let i = 0; i < label.length && qi < lower.length; i++) {
          if (label[i] === lower[qi]) qi++;
        }
        if (qi === lower.length) score += 25;
      }

      return { cmd, score };
    })
    .filter((item) => item.score > 0)
    .sort((a, b) => b.score - a.score);

  return scored.map((item) => item.cmd);
}

/**
 * Quick-connect: match a typed hostname against saved profiles.
 * If no match, offer to create a new connection.
 */
export function quickConnect(
  profiles: SshProfile[],
  input: string,
): { type: "existing"; profile: SshProfile } | { type: "new"; hostname: string } | null {
  if (!input.trim()) return null;

  // Try to match against existing profiles
  const match = profiles.find(
    (p) =>
      p.host.toLowerCase() === input.toLowerCase() ||
      p.label.toLowerCase() === input.toLowerCase(),
  );

  if (match) {
    return { type: "existing", profile: match };
  }

  // Looks like a hostname — offer to create new
  if (/^[a-zA-Z0-9.-]+$/.test(input)) {
    return { type: "new", hostname: input };
  }

  return null;
}

// ── Keyboard Shortcuts ───────────────────────────────────────────

export const SSH_KEYBOARD_SHORTCUTS = {
  openSshView: { key: "S", modifiers: ["Cmd", "Shift"] },
  openRemoteTerminal: { key: "T", modifiers: ["Cmd", "Shift"] },
} as const;

/**
 * Menu bar entries for File -> SSH submenu.
 */
export const SSH_MENU_ITEMS = [
  { label: "Connect...", accelerator: undefined, action: "ssh.connect" },
  { label: "Disconnect All", accelerator: undefined, action: "ssh.disconnect-all" },
  { type: "separator" as const },
  { label: "Open SSH View", accelerator: "CmdOrCtrl+Shift+S", action: "ssh.open-view" },
  { label: "Open Remote Terminal", accelerator: "CmdOrCtrl+Shift+T", action: "ssh.open-terminal" },
] as const;
