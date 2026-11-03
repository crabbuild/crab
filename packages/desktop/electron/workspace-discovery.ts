import * as fs from "node:fs/promises";
import * as path from "node:path";
import { minimatch } from "minimatch";

export interface DiscoveryOptions {
  rootPath: string;
  maxDepth: number;
  excludePatterns: string[];
}

export interface DiscoveredRepo {
  /** Absolute path to the repo root (parent of .git) */
  absolutePath: string;
  /** Path relative to workspace root */
  relativePath: string;
  /** Display name (directory basename) */
  name: string;
}

const DEFAULT_MAX_DEPTH = 3;
const DEFAULT_EXCLUDE_PATTERNS = [
  "node_modules",
  ".git",
  "vendor",
  "target",
  "build",
];

/**
 * Scans rootPath for .git directories up to maxDepth levels deep.
 * Returns discovered repos, filtered by exclude patterns and removed list,
 * merged with manually added repos.
 */
export async function discoverRepos(
  options: Partial<DiscoveryOptions> & { rootPath: string },
  config?: { added?: string[]; removed?: string[] },
): Promise<DiscoveredRepo[]> {
  const maxDepth = options.maxDepth ?? DEFAULT_MAX_DEPTH;
  const excludePatterns =
    options.excludePatterns ?? [...DEFAULT_EXCLUDE_PATTERNS];
  const rootPath = path.resolve(options.rootPath);

  const removed = new Set(config?.removed ?? []);
  const added = config?.added ?? [];

  const discovered: DiscoveredRepo[] = [];

  await scanDirectory(rootPath, rootPath, 0, maxDepth, excludePatterns, discovered);

  // Filter out removed repos
  const filtered = discovered.filter(
    (repo) => !removed.has(repo.relativePath),
  );

  // Include manually added repos (resolve relative paths against workspace root)
  const addedRepos: DiscoveredRepo[] = [];
  for (const relPath of added) {
    const absolutePath = path.resolve(rootPath, relPath);
    const relativePath = path.relative(rootPath, absolutePath);
    // Skip if already discovered or in removed list
    if (removed.has(relativePath)) continue;
    if (filtered.some((r) => r.relativePath === relativePath)) continue;

    addedRepos.push({
      absolutePath,
      relativePath,
      name: path.basename(absolutePath),
    });
  }

  return [...filtered, ...addedRepos];
}

function isExcluded(dirName: string, excludePatterns: string[]): boolean {
  for (const pattern of excludePatterns) {
    if (minimatch(dirName, pattern)) {
      return true;
    }
  }
  return false;
}

async function scanDirectory(
  currentPath: string,
  rootPath: string,
  currentDepth: number,
  maxDepth: number,
  excludePatterns: string[],
  results: DiscoveredRepo[],
): Promise<void> {
  if (currentDepth > maxDepth) return;

  let entries: import("node:fs").Dirent[];
  try {
    entries = await fs.readdir(currentPath, { withFileTypes: true });
  } catch {
    // Permission denied or other read errors — skip silently
    return;
  }

  // Check if this directory itself is a git repo (has a .git subdirectory)
  const hasGitDir = entries.some(
    (e) => e.name === ".git" && (e.isDirectory() || e.isFile()),
  );

  if (hasGitDir && currentPath !== rootPath) {
    const relativePath = path.relative(rootPath, currentPath);
    results.push({
      absolutePath: currentPath,
      relativePath,
      name: path.basename(currentPath),
    });
    // Don't recurse into discovered repos — they are leaf nodes
    return;
  }

  // Recurse into subdirectories
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    if (isExcluded(entry.name, excludePatterns)) continue;

    const childPath = path.join(currentPath, entry.name);
    await scanDirectory(
      childPath,
      rootPath,
      currentDepth + 1,
      maxDepth,
      excludePatterns,
      results,
    );
  }
}
