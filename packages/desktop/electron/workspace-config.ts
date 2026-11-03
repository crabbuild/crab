import * as fs from "node:fs/promises";
import * as path from "node:path";

export interface WorkspaceConfig {
  version: 1;
  discovery: {
    maxDepth: number;
    excludePatterns: string[];
  };
  repos: {
    /** Manually removed repos (excluded from discovery, relative paths) */
    removed: string[];
  };
  display: {
    tabBarState: string[];
  };
}

const CONFIG_DIR = ".crab";
const CONFIG_FILE = "workspace.json";

const DEFAULT_EXCLUDE_PATTERNS = [
  "node_modules",
  ".git",
  "vendor",
  "target",
  "build",
];

/** Returns a default WorkspaceConfig with sensible defaults. */
export function defaultWorkspaceConfig(): WorkspaceConfig {
  return {
    version: 1,
    discovery: {
      maxDepth: 3,
      excludePatterns: [...DEFAULT_EXCLUDE_PATTERNS],
    },
    repos: {
      removed: [],
    },
    display: {
      tabBarState: [],
    },
  };
}

/**
 * Reads and parses `.crab/workspace.json` from the given root path.
 *
 * Returns null if the file does not exist.
 * Returns default config and logs a warning if the file is malformed JSON.
 */
export async function readWorkspaceConfig(
  rootPath: string,
): Promise<WorkspaceConfig | null> {
  const configPath = path.join(rootPath, CONFIG_DIR, CONFIG_FILE);

  let raw: string;
  try {
    raw = await fs.readFile(configPath, "utf-8");
  } catch (err: unknown) {
    if (isNodeError(err) && err.code === "ENOENT") {
      return null;
    }
    // Other read errors (permissions, etc.) — treat as missing
    return null;
  }

  try {
    const parsed = JSON.parse(raw);
    return validateConfig(parsed);
  } catch {
    console.warn(
      `[workspace-config] Malformed workspace config at ${configPath}, using defaults`,
    );
    return defaultWorkspaceConfig();
  }
}

/**
 * Serializes and writes a WorkspaceConfig to `.crab/workspace.json`.
 *
 * Creates the `.crab/` directory if it does not exist.
 */
export async function writeWorkspaceConfig(
  rootPath: string,
  config: WorkspaceConfig,
): Promise<void> {
  const dirPath = path.join(rootPath, CONFIG_DIR);
  await fs.mkdir(dirPath, { recursive: true });

  const configPath = path.join(dirPath, CONFIG_FILE);
  const content = JSON.stringify(config, null, 2) + "\n";
  await fs.writeFile(configPath, content, "utf-8");
}

/**
 * Validates a parsed JSON object against the WorkspaceConfig shape.
 * Returns the config if valid, otherwise throws to trigger the malformed fallback.
 *
 * Backward compatible: accepts old configs with `repos.added` field
 * (ignores it since we no longer support adding repos from outside
 * the workspace root).
 */
function validateConfig(obj: unknown): WorkspaceConfig {
  if (typeof obj !== "object" || obj === null) {
    throw new Error("Config is not an object");
  }

  const record = obj as Record<string, unknown>;

  if (record.version !== 1) {
    throw new Error("Unsupported config version");
  }

  const discovery = record.discovery;
  if (typeof discovery !== "object" || discovery === null) {
    throw new Error("Missing discovery section");
  }
  const disc = discovery as Record<string, unknown>;
  if (typeof disc.maxDepth !== "number" || disc.maxDepth < 1) {
    throw new Error("Invalid maxDepth");
  }
  if (!Array.isArray(disc.excludePatterns)) {
    throw new Error("Invalid excludePatterns");
  }

  const repos = record.repos;
  if (typeof repos !== "object" || repos === null) {
    throw new Error("Missing repos section");
  }
  const r = repos as Record<string, unknown>;
  if (!Array.isArray(r.removed)) {
    throw new Error("Invalid repos.removed");
  }

  const display = record.display;
  if (typeof display !== "object" || display === null) {
    throw new Error("Missing display section");
  }
  const d = display as Record<string, unknown>;
  if (!Array.isArray(d.tabBarState)) {
    throw new Error("Invalid display.tabBarState");
  }

  return {
    version: 1,
    discovery: {
      maxDepth: disc.maxDepth as number,
      excludePatterns: (disc.excludePatterns as unknown[]).map(String),
    },
    repos: {
      removed: (r.removed as unknown[]).map(String),
    },
    display: {
      tabBarState: (d.tabBarState as unknown[]).map(String),
    },
  };
}

function isNodeError(err: unknown): err is NodeJS.ErrnoException {
  return err instanceof Error && "code" in err;
}
