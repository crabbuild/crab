import * as path from "node:path";
import { detectAgents } from "./orchestrator-adapters";
import { loadAgentConfig } from "./orchestrator-settings";

/** Resolve a preset against the same host configuration used by agent detection. */
export function registerTerminalAgentCommand(
  ipcMain: { handle(channel: string, handler: (...args: unknown[]) => unknown): void },
  defaultShell: () => string,
): void {
  ipcMain.handle("crab:term-agent-command", async (_event, value) => {
    const { agentType, command } = value as { agentType: string; command: string };
    const agent = (await detectAgents()).find(candidate => candidate.type === agentType);
    if (!agent?.available || !agent.path) throw new Error("The selected agent CLI is not executable. Check its installation or configured path.");
    if (command !== agent.binary && !command.startsWith(`${agent.binary} `)) throw new Error("The terminal preset does not match the selected agent CLI.");
    const config = loadAgentConfig(agentType);
    const shell = path.basename(defaultShell()).toLowerCase();
    const powershell = /^(?:powershell|pwsh)(?:\.exe)?$/.test(shell);
    const cmd = /^cmd(?:\.exe)?$/.test(shell);
    const quote = (argument: string) => cmd
      ? `"${argument.replaceAll('"', '""')}"`
      : powershell ? `'${argument.replaceAll("'", "''")}'` : `'${argument.replaceAll("'", "'\\''")}'`;
    // Quoted paths require PowerShell's call operator. POSIX shells and cmd
    // execute the quoted path directly; omitting this breaks paths with spaces.
    const invocation = `${powershell ? "& " : ""}${quote(agent.path)}`;
    const args = (config.extraArgs ?? []).map(quote).join(" ");
    const suffix = command.slice(agent.binary.length).trimStart();
    return { command: [invocation, args, suffix].filter(Boolean).join(" "), env: config.env };
  });
}
