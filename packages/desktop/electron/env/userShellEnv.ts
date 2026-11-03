import os from "node:os";
import path from "node:path";


export type UserShellEnvOptions = {
  projectPath?: string;
  extraEnv?: NodeJS.ProcessEnv;
};

let cachedBaseEnv: NodeJS.ProcessEnv | null = null;

function uniqPathEntries(value: string): string {
  const seen = new Set<string>();
  const result: string[] = [];

  for (const entry of value.split(path.delimiter)) {
    if (!entry || seen.has(entry)) continue;
    seen.add(entry);
    result.push(entry);
  }

  return result.join(path.delimiter);
}

function defaultPath(): string {
  if (process.platform === "darwin") {
    return [
      "/opt/homebrew/bin",
      "/opt/homebrew/sbin",
      "/usr/local/bin",
      "/usr/bin",
      "/bin",
      "/usr/sbin",
      "/sbin",
    ].join(path.delimiter);
  }

  if (process.platform === "linux") {
    return [
      "/usr/local/bin",
      "/usr/bin",
      "/bin",
      "/usr/sbin",
      "/sbin",
    ].join(path.delimiter);
  }

  return process.env.PATH ?? "";
}

function mergeNoProxy(value?: string): string {
  const entries = [
    value,
    "localhost",
    "127.0.0.1",
    "::1",
  ]
    .filter(Boolean)
    .flatMap((v) => String(v).split(","))
    .map((v) => v.trim())
    .filter(Boolean);

  return Array.from(new Set(entries)).join(",");
}

export async function getUserShellBaseEnv(): Promise<NodeJS.ProcessEnv> {
  if (cachedBaseEnv) return cachedBaseEnv;

  let loginShellEnv: NodeJS.ProcessEnv = {};

  try {
    const { shellEnv } = await import("shell-env");
    // Reads the user's default shell environment.
    loginShellEnv = await shellEnv();
  } catch (error) {
    console.warn("[env] Failed to read login shell env:", error);
  }

  const home = os.homedir();
  const shell =
    loginShellEnv.SHELL ||
    process.env.SHELL ||
    (process.platform === "win32" ? "powershell.exe" : "/bin/zsh");

  const pathValue = uniqPathEntries(
    [
      loginShellEnv.PATH,
      process.env.PATH,
      defaultPath(),
    ]
      .filter(Boolean)
      .join(path.delimiter)
  );

  cachedBaseEnv = {
    ...process.env,
    ...loginShellEnv,

    HOME: home,
    USER: loginShellEnv.USER || process.env.USER,
    LOGNAME: loginShellEnv.LOGNAME || process.env.LOGNAME,
    SHELL: shell,

    PATH: pathValue,
    TERM: "xterm-256color",
    COLORTERM: "truecolor",

    // Important when cc-switch / provider proxy uses localhost.
    NO_PROXY: mergeNoProxy(loginShellEnv.NO_PROXY || process.env.NO_PROXY),
    no_proxy: mergeNoProxy(loginShellEnv.no_proxy || process.env.no_proxy),
  };

  return cachedBaseEnv;
}

export async function buildTerminalEnv(
  options: UserShellEnvOptions = {}
): Promise<NodeJS.ProcessEnv> {
  const base = await getUserShellBaseEnv();

  return {
    ...base,
    ...options.extraEnv,

    // Let spawned tools know they are inside Crab.
    CRAB_TERMINAL: "1",

    // Preserve cwd-related behavior.
    PWD: options.projectPath || base.PWD || os.homedir(),
  };
}

export function invalidateUserShellEnvCache(): void {
  cachedBaseEnv = null;
}
