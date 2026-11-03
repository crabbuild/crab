/**
 * Pluggable commit message generation interface.
 *
 * Two providers ship: `local-llama` (calls a user-provided local model
 * endpoint) and `openai-compatible` (any provider with POST
 * /v1/chat/completions). Off by default; a "Suggest" button appears in
 * the composer once configured. Never auto-commits.
 */

// ── Types ───────────────────────────────────────────────────────────

export interface CommitMessageContext {
  repoPath: string;
  stagedPatch: string;
  recentSubjects: string[];
}

export interface CommitMessageGenerator {
  id: string;
  name: string;
  suggest(ctx: CommitMessageContext): Promise<string>;
}

// ── Registry ────────────────────────────────────────────────────────

const generators = new Map<string, CommitMessageGenerator>();

/**
 * Register a generator implementation. Overwrites any existing
 * generator with the same `id`.
 */
export function registerGenerator(gen: CommitMessageGenerator): void {
  generators.set(gen.id, gen);
}

/**
 * Remove a generator from the registry.
 */
export function unregisterGenerator(id: string): void {
  generators.delete(id);
}

/**
 * Returns the currently configured generator based on settings,
 * or `null` when AI suggestions are disabled.
 */
export function getConfiguredGenerator(
  aiProvider: string | null | undefined,
): CommitMessageGenerator | null {
  if (!aiProvider || aiProvider === "none") return null;
  return generators.get(aiProvider) ?? null;
}

/**
 * Returns all registered generator IDs.
 */
export function getRegisteredGeneratorIds(): string[] {
  return Array.from(generators.keys());
}
