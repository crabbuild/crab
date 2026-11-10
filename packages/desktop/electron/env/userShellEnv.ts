import os from "node:os";
import path from "node:path";


export type UserShellEnvOptions = {
  projectPath?: string;
  extraEnv?: NodeJS.ProcessEnv;
};

let cachedBaseEnv: Promise<NodeJS.ProcessEnv> | null = null;

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

export function getUserShellBaseEnv(): Promise<NodeJS.ProcessEnv> {
  if (!cachedBaseEnv) {
    const pending = loadUserShellBaseEnv();
    cachedBaseEnv = pending;
    void pending.catch(() => { if (cachedBaseEnv === pending) cachedBaseEnv = null; });
  }
  return cachedBaseEnv;
}

async function loadUserShellBaseEnv(): Promise<NodeJS.ProcessEnv> {
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

  return {
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

export async function getExpandedEnv(extraEnv?: Record<string, string>): Promise<Record<string, string>> {
  const base = await getUserShellBaseEnv();
  const env = Object.fromEntries(Object.entries({ ...base, ...process.env, ...extraEnv })
    .filter((entry): entry is [string, string] => typeof entry[1] === "string"));
  const home = os.homedir();
  const fs = require("node:fs");

  const extraPaths: string[] = [
    path.join(home, ".cargo", "bin"),
    "/usr/local/bin",
    "/opt/homebrew/bin",
    path.join(home, ".local", "bin"),
    path.join(home, ".local", "share", "mise", "shims"),  // mise shims
    path.join(home, ".npm-global", "bin"),
    "/opt/homebrew/opt/node/bin",
  ];

  // Detect nvm in all common locations ($NVM_DIR, ~/.nvm, ~/.config/nvm).
  const nvmCandidates = [
    process.env.NVM_DIR,
    path.join(home, ".nvm"),
    path.join(home, ".config", "nvm"),
  ].filter(Boolean) as string[];

  for (const nvmDir of nvmCandidates) {
    try {
      const versions = path.join(nvmDir, "versions", "node");
      if (!fs.existsSync(versions)) continue;
      const dirs = fs.readdirSync(versions) as string[];
      // Add every installed node version — binaries may be in any of them.
      // Put the newest first so it wins duplicate lookups.
      const sorted = dirs.sort().reverse();
      for (const v of sorted) {
        extraPaths.push(path.join(versions, v, "bin"));
      }
    } catch {
      // Best-effort.
    }
  }

  // Detect mise-installed node versions and add their bin dirs too.
  try {
    const miseNode = path.join(home, ".local", "share", "mise", "installs", "node");
    if (fs.existsSync(miseNode)) {
      const dirs = fs.readdirSync(miseNode) as string[];
      for (const v of dirs.sort().reverse()) {
        extraPaths.push(path.join(miseNode, v, "bin"));
      }
    }
  } catch {
    // Best-effort.
  }

  const currentPath = env.PATH ?? "/usr/bin:/bin:/usr/sbin:/sbin";
  const pathSet = new Set(currentPath.split(path.delimiter));

  // Merge login-shell PATH (picks up nvm/volta/asdf/bun/custom prefixes).
  const shellPath = base.PATH;
  if (shellPath) {
    for (const p of shellPath.split(path.delimiter)) {
      if (p) pathSet.add(p);
    }
  }

  for (const p of extraPaths) {
    if (!pathSet.has(p)) pathSet.add(p);
  }
  env.PATH = [...pathSet].join(path.delimiter);
  return env;
}
