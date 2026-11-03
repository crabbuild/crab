/**
 * Instructions Library — user-managed prompt templates, global across repos.
 *
 * Users build a personal library of frequently-used prompts (e.g.
 * "Review this PR for security issues", "Write tests for the given function").
 * They can search, pin favorites, edit, and delete entries. The library
 * is stored in `localStorage` so it survives restarts and is instantly
 * available — no IPC round-trip when the user opens the picker.
 */

// ── Types ───────────────────────────────────────────────────────────

export interface Instruction {
  id: string;
  /** Short label shown in the picker (e.g. "Security review"). */
  title: string;
  /** The prompt body that gets inserted into the composer. */
  body: string;
  /** Optional category for grouping (e.g. "review", "test", "refactor"). */
  category?: string;
  /** When true, the instruction appears at the top of the picker. */
  pinned: boolean;
  /** Usage counter — we sort unpinned entries by most-used then recency. */
  useCount: number;
  /** Last time this instruction was inserted, as a Unix ms timestamp. */
  lastUsedAt: number;
  createdAt: number;
  updatedAt: number;
}

// ── Storage ─────────────────────────────────────────────────────────

const STORAGE_KEY = "crab.instructionsLibrary.v1";

function load(): Instruction[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return seedDefaults();
    const parsed = JSON.parse(raw) as Instruction[];
    return Array.isArray(parsed) ? parsed : seedDefaults();
  } catch {
    return seedDefaults();
  }
}

function save(list: Instruction[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(list));
  } catch {
    // Quota or serialization failure — best-effort.
  }
}

/**
 * Ships a handful of useful starter instructions so the picker
 * isn't empty on first open. Users can pin/edit/delete them freely.
 */
function seedDefaults(): Instruction[] {
  const now = Date.now();
  const base: Omit<Instruction, "id">[] = [
    {
      title: "Plan before you code",
      body:
        "Before making any changes, propose a concise implementation plan. " +
        "List the files you'll touch, the approach, key risks, and what you'll test. " +
        "Wait for my approval before editing.",
      category: "planning",
      pinned: true,
      useCount: 0,
      lastUsedAt: now,
      createdAt: now,
      updatedAt: now,
    },
    {
      title: "Security review",
      body:
        "Review the current changes for security issues: input validation, " +
        "authentication/authorization, injection risks, secret leakage, and " +
        "insecure defaults. Report findings with file + line references and " +
        "suggested fixes.",
      category: "review",
      pinned: false,
      useCount: 0,
      lastUsedAt: now,
      createdAt: now,
      updatedAt: now,
    },
    {
      title: "Write tests for this",
      body:
        "Add unit tests for the code I just referenced. Cover the happy path " +
        "and edge cases (empty input, error conditions, boundary values). Use " +
        "the project's existing test framework and match the style of neighboring tests.",
      category: "test",
      pinned: false,
      useCount: 0,
      lastUsedAt: now,
      createdAt: now,
      updatedAt: now,
    },
    {
      title: "Explain this code",
      body:
        "Walk me through the code I referenced. Cover: what it does end-to-end, " +
        "the key data flow, any non-obvious design decisions, and anything that " +
        "looks suspicious or brittle.",
      category: "explain",
      pinned: false,
      useCount: 0,
      lastUsedAt: now,
      createdAt: now,
      updatedAt: now,
    },
    {
      title: "Refactor without changing behavior",
      body:
        "Refactor the referenced code for clarity without changing its " +
        "observable behavior. Keep the public API the same, preserve tests, " +
        "and explain what you changed and why.",
      category: "refactor",
      pinned: false,
      useCount: 0,
      lastUsedAt: now,
      createdAt: now,
      updatedAt: now,
    },
  ];
  return base.map((entry, i) => ({
    ...entry,
    id: `seed-${i}-${now}`,
  }));
}

// ── Public API ──────────────────────────────────────────────────────

export function listInstructions(): Instruction[] {
  return load();
}

/**
 * Sorts pinned-first, then by usage/recency. Callers typically pass
 * the output of `listInstructions()` through this for display.
 */
export function sortInstructions(list: Instruction[]): Instruction[] {
  return [...list].sort((a, b) => {
    if (a.pinned !== b.pinned) return a.pinned ? -1 : 1;
    if (b.useCount !== a.useCount) return b.useCount - a.useCount;
    return b.lastUsedAt - a.lastUsedAt;
  });
}

export function searchInstructions(query: string): Instruction[] {
  const all = sortInstructions(load());
  const q = query.trim().toLowerCase();
  if (!q) return all;
  return all.filter(
    (i) =>
      i.title.toLowerCase().includes(q) ||
      i.body.toLowerCase().includes(q) ||
      (i.category ?? "").toLowerCase().includes(q),
  );
}

export function createInstruction(
  input: { title: string; body: string; category?: string; pinned?: boolean },
): Instruction {
  const now = Date.now();
  const next: Instruction = {
    id: `ins-${now}-${Math.random().toString(36).slice(2, 8)}`,
    title: input.title.trim(),
    body: input.body.trim(),
    category: input.category?.trim() || undefined,
    pinned: input.pinned ?? false,
    useCount: 0,
    lastUsedAt: now,
    createdAt: now,
    updatedAt: now,
  };
  const all = load();
  save([next, ...all]);
  return next;
}

export function updateInstruction(
  id: string,
  patch: Partial<Pick<Instruction, "title" | "body" | "category" | "pinned">>,
): Instruction | null {
  const all = load();
  const idx = all.findIndex((i) => i.id === id);
  if (idx < 0) return null;
  const current = all[idx];
  const next: Instruction = {
    ...current,
    ...patch,
    updatedAt: Date.now(),
  };
  all[idx] = next;
  save(all);
  return next;
}

export function deleteInstruction(id: string): boolean {
  const all = load();
  const next = all.filter((i) => i.id !== id);
  if (next.length === all.length) return false;
  save(next);
  return true;
}

export function togglePinned(id: string): Instruction | null {
  const all = load();
  const idx = all.findIndex((i) => i.id === id);
  if (idx < 0) return null;
  return updateInstruction(id, { pinned: !all[idx].pinned });
}

/**
 * Mark an instruction as used — increments `useCount` and bumps
 * `lastUsedAt`. Called after the user inserts it into the composer.
 */
export function recordUsage(id: string): void {
  const all = load();
  const idx = all.findIndex((i) => i.id === id);
  if (idx < 0) return;
  all[idx] = {
    ...all[idx],
    useCount: all[idx].useCount + 1,
    lastUsedAt: Date.now(),
  };
  save(all);
}

/** Resets the library to the default seeds. Used by tests and the settings UI. */
export function resetLibrary(): void {
  save(seedDefaults());
}
