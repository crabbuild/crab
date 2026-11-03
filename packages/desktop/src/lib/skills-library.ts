/**
 * Skills Library — user-managed reusable expertise packages.
 *
 * A skill is a Markdown document (conventionally `SKILL.md` or `*.md`
 * at a given path) that captures domain knowledge the agent should load
 * when invoked. Users install skills from GitHub URLs and can reference
 * them via @ in the composer.
 *
 * Stored in localStorage as a list of installed skill manifests. The raw
 * markdown bodies are fetched lazily when an agent needs them — we keep
 * only metadata + cached bodies here to avoid quota issues.
 */

// ── Types ───────────────────────────────────────────────────────────

export interface Skill {
  id: string;
  /** Short name shown in the UI (from front-matter or filename). */
  name: string;
  /** One-liner description (from front-matter). */
  description: string;
  /** Source GitHub URL the skill was installed from. */
  sourceUrl: string;
  /** Raw markdown body (cached for offline use). */
  body: string;
  /** Word/character counts for quick UX display. */
  length: number;
  installedAt: number;
  updatedAt: number;
}

// ── Storage ─────────────────────────────────────────────────────────

const STORAGE_KEY = "crab.skillsLibrary.v1";

function load(): Skill[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as Skill[];
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

function save(list: Skill[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(list));
  } catch {
    // Quota exceeded — this can happen if a user installs a very large
    // skill. We surface it to the caller so they can display a toast.
    throw new Error("Unable to save skills — storage quota exceeded");
  }
}

// ── Public API ──────────────────────────────────────────────────────

export function listSkills(): Skill[] {
  return load().sort((a, b) => a.name.localeCompare(b.name));
}

export function searchSkills(query: string): Skill[] {
  const all = listSkills();
  const q = query.trim().toLowerCase();
  if (!q) return all;
  return all.filter(
    (s) =>
      s.name.toLowerCase().includes(q) ||
      s.description.toLowerCase().includes(q),
  );
}

export function findSkillByName(name: string): Skill | null {
  const normalized = name.trim().toLowerCase();
  return load().find((s) => s.name.toLowerCase() === normalized) ?? null;
}

export function removeSkill(id: string): boolean {
  const all = load();
  const next = all.filter((s) => s.id !== id);
  if (next.length === all.length) return false;
  save(next);
  return true;
}

// ── GitHub URL → raw markdown ──────────────────────────────────────

/**
 * Converts a GitHub URL to its raw.githubusercontent equivalent.
 *
 * Accepts:
 *   - https://github.com/owner/repo/blob/<ref>/path/to/file.md
 *   - https://github.com/owner/repo/tree/<ref>/path/to/folder   (resolves to folder/SKILL.md)
 *   - https://raw.githubusercontent.com/owner/repo/<ref>/path/to/file.md (passed through)
 *   - owner/repo/path/to/file.md (implicit GitHub shorthand)
 */
export function githubUrlToRaw(inputUrl: string): string | null {
  const s = inputUrl.trim();
  if (!s) return null;

  // Already a raw URL → pass through.
  if (s.startsWith("https://raw.githubusercontent.com/")) {
    return s;
  }

  // Parse standard github.com URL.
  const ghMatch = s.match(
    /^https?:\/\/(?:www\.)?github\.com\/([^/]+)\/([^/]+)\/(blob|tree)\/([^/]+)\/(.+)$/i,
  );
  if (ghMatch) {
    const [, owner, repo, kind, ref, rest] = ghMatch;
    const cleanPath = rest.split("#")[0].split("?")[0];
    const resolved = kind === "tree"
      ? cleanPath.replace(/\/+$/, "") + "/SKILL.md"
      : cleanPath;
    return `https://raw.githubusercontent.com/${owner}/${repo}/${ref}/${resolved}`;
  }

  // Shorthand: owner/repo/path
  const shorthand = s.match(/^([^/\s]+)\/([^/\s]+)\/(.+)$/);
  if (shorthand) {
    const [, owner, repo, rest] = shorthand;
    return `https://raw.githubusercontent.com/${owner}/${repo}/main/${rest}`;
  }

  return null;
}

/**
 * Parses front-matter from a skill markdown body.
 *
 * Supports the minimal YAML-ish subset we care about:
 *   - `name:`    (falls back to the first `# heading` if absent)
 *   - `description:` (falls back to the first non-empty paragraph)
 */
function parseSkillFrontmatter(
  body: string,
  fallbackName: string,
): { name: string; description: string; content: string } {
  let name = fallbackName;
  let description = "";
  let content = body;

  const fmMatch = body.match(/^---\n([\s\S]*?)\n---\n?/);
  if (fmMatch) {
    const block = fmMatch[1];
    for (const line of block.split(/\r?\n/)) {
      const kv = line.match(/^([A-Za-z][A-Za-z0-9_-]*):\s*(.+)$/);
      if (!kv) continue;
      const [, key, value] = kv;
      const v = value.replace(/^["'](.*)["']$/, "$1").trim();
      if (key.toLowerCase() === "name") name = v;
      else if (key.toLowerCase() === "description") description = v;
    }
    content = body.slice(fmMatch[0].length);
  }

  // Fallbacks.
  if (!description) {
    const firstPara = content
      .split(/\n\s*\n/)
      .map((p) => p.trim())
      .find((p) => p && !p.startsWith("#") && !p.startsWith("---"));
    description = firstPara?.slice(0, 160) ?? "";
  }
  if (name === fallbackName) {
    const h1 = content.match(/^#\s+(.+)$/m);
    if (h1) name = h1[1].trim();
  }

  return { name, description, content };
}

/**
 * Install a skill from a GitHub URL. Returns the installed skill, or
 * throws with a human-readable error message.
 */
export async function installSkillFromUrl(inputUrl: string): Promise<Skill> {
  const raw = githubUrlToRaw(inputUrl);
  if (!raw) {
    throw new Error(
      "Could not understand that URL. Use a GitHub blob/tree URL, a raw.githubusercontent URL, or `owner/repo/path/to/SKILL.md`.",
    );
  }

  let response: Response;
  try {
    response = await fetch(raw);
  } catch (err) {
    throw new Error(`Network error fetching skill: ${(err as Error).message}`);
  }
  if (!response.ok) {
    throw new Error(`Fetch failed with HTTP ${response.status} ${response.statusText}`);
  }

  const body = await response.text();
  if (!body.trim()) {
    throw new Error("The skill file is empty.");
  }

  const fallbackName =
    raw.split("/").pop()?.replace(/\.md$/i, "") ?? "Untitled skill";
  const { name, description, content } = parseSkillFrontmatter(body, fallbackName);

  const now = Date.now();
  const skill: Skill = {
    id: `skill-${now}-${Math.random().toString(36).slice(2, 8)}`,
    name,
    description,
    sourceUrl: inputUrl,
    body: content,
    length: content.length,
    installedAt: now,
    updatedAt: now,
  };

  // Replace any existing skill with the same name (update-in-place).
  const all = load().filter((s) => s.name.toLowerCase() !== name.toLowerCase());
  save([...all, skill]);
  return skill;
}

/** Re-fetches a skill's body from its source URL. */
export async function refreshSkill(id: string): Promise<Skill | null> {
  const all = load();
  const existing = all.find((s) => s.id === id);
  if (!existing) return null;
  const updated = await installSkillFromUrl(existing.sourceUrl);
  return updated;
}
