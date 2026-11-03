/**
 * Pure-function module for constructing structured agent prompts from card data.
 *
 * Extracts file paths from card descriptions and builds a formatted prompt
 * with repository, labels, related files, and task sections. Enforces an
 * 8000-character limit by truncating the description while preserving
 * metadata sections.
 */

export interface AgentContextInput {
  cardTitle: string;
  cardDescription: string;
  repoPath: string;
  labels: string[];
}

export interface AgentContext {
  prompt: string;
  repoPath: string;
  relatedFiles: string[];
}

const MAX_PROMPT_LENGTH = 8000;

/**
 * Extracts file paths from text using a regex matching paths with file extensions.
 *
 * Matches patterns like `src/foo/bar.ts`, `./lib/utils.py`, `electron/kanban/file.ts`.
 * Requires at least one directory separator and a file extension.
 */
export function extractFilePaths(text: string): string[] {
  if (!text) return [];

  // Match paths that have at least one slash and end with a file extension.
  // Supports optional leading ./ or ../
  const pattern = /(?:\.{0,2}\/)?(?:[\w.@-]+\/)+[\w.-]+\.\w+/g;
  const matches = text.match(pattern);
  if (!matches) return [];

  // Deduplicate while preserving order
  return [...new Set(matches)];
}

/**
 * Builds a structured prompt from card data.
 *
 * Prompt format:
 * ```
 * ## Repository
 * {repoPath}
 *
 * ## Labels
 * - {label1}
 * - {label2}
 *
 * ## Related Files
 * - {filePath1}
 * - {filePath2}
 *
 * ## Task
 * {cardTitle}
 *
 * {cardDescription}
 * ```
 *
 * If description is empty, the "Related Files" section is omitted.
 * Truncates the description portion to keep the total prompt under 8000 characters.
 */
export function buildAgentContext(input: AgentContextInput): AgentContext {
  const { cardTitle, cardDescription, repoPath, labels } = input;

  const relatedFiles = extractFilePaths(cardDescription);

  // Build metadata sections (always preserved)
  const repoSection = `## Repository\n${repoPath}`;

  const labelsSection = labels.length > 0
    ? `## Labels\n${labels.map((l) => `- ${l}`).join("\n")}`
    : "## Labels\n(none)";

  const filesSection = cardDescription && relatedFiles.length > 0
    ? `## Related Files\n${relatedFiles.map((f) => `- ${f}`).join("\n")}`
    : "";

  const taskHeader = `## Task\n${cardTitle}`;

  // Assemble the metadata portion (everything except the description body)
  const metadataParts = [repoSection, labelsSection];
  if (filesSection) {
    metadataParts.push(filesSection);
  }
  metadataParts.push(taskHeader);

  const metadataBlock = metadataParts.join("\n\n");

  // If description is empty, the prompt is just the metadata
  if (!cardDescription) {
    const prompt = metadataBlock;
    return {
      prompt: prompt.slice(0, MAX_PROMPT_LENGTH),
      repoPath,
      relatedFiles,
    };
  }

  // Full prompt with description
  const fullPrompt = `${metadataBlock}\n\n${cardDescription}`;

  if (fullPrompt.length <= MAX_PROMPT_LENGTH) {
    return { prompt: fullPrompt, repoPath, relatedFiles };
  }

  // Truncate description to fit within the limit.
  // Reserve space for metadata + separator + ellipsis marker.
  const separator = "\n\n";
  const truncationMarker = "\n\n[...truncated]";
  const availableForDescription =
    MAX_PROMPT_LENGTH - metadataBlock.length - separator.length - truncationMarker.length;

  const truncatedDescription = availableForDescription > 0
    ? cardDescription.slice(0, availableForDescription) + truncationMarker
    : truncationMarker;

  const prompt = `${metadataBlock}${separator}${truncatedDescription}`;

  return {
    prompt: prompt.slice(0, MAX_PROMPT_LENGTH),
    repoPath,
    relatedFiles,
  };
}
