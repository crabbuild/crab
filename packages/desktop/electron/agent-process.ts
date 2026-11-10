import { spawn } from "node:child_process";
import type { EventEmitter } from "node:events";
import type { Readable, Writable } from "node:stream";
import { getExpandedEnv } from "./orchestrator-adapters";

export interface AgentProcess extends EventEmitter {
  stdin: Writable | null;
  stdout: Readable | null;
  stderr: Readable | null;
  kill(signal?: NodeJS.Signals | number): boolean;
}

export interface OwnedAgentProcess {
  process: AgentProcess;
  closed: Promise<void>;
}

export type AgentProcessFactory = (binary: string, args: string[], options: {
  cwd: string;
  env?: Record<string, string>;
}) => OwnedAgentProcess;

/** Expands local credentials/PATH only at the local process boundary. */
export const spawnLocalAgent: AgentProcessFactory = (binary, args, options) => {
  const env = getExpandedEnv(options.env);
  const process = spawn(binary, args, { cwd: options.cwd, env, stdio: ["pipe", "pipe", "pipe"] });
  const closed = new Promise<void>(resolve => { process.once("close", () => resolve()); });
  return { process, closed };
};
