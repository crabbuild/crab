/**
 * Agent identity → left-border accent colour mapping.
 *
 * Used by the chat message column to render a 1–2 px left-border accent
 * coloured by the adapter's identity (the `name` field). The CSS custom
 * property names reference the design-token palette in index.css.
 */

/**
 * Maps an adapter name to the CSS custom property that should colour
 * the left-border accent on agent messages.
 *
 * Returns a CSS colour value using the design-token RGB triples so
 * callers can use it directly in inline styles.
 */
export function agentBorderColor(adapterName: string | undefined | null): string {
  const name = (adapterName ?? "").toLowerCase();

  if (name.includes("codex") || name.includes("openai") || name.startsWith("gpt-")) {
    // Codex → --primary
    return "rgb(var(--primary))";
  }

  if (name.includes("claude") || name.includes("anthropic")) {
    // Claude Code → --thinking
    return "rgb(var(--thinking))";
  }

  // Generic / unknown → --muted-foreground
  return "rgb(var(--muted-foreground))";
}

/**
 * Returns a Tailwind-compatible class for the left-border accent colour.
 * Useful when inline styles aren't practical.
 */
export function agentBorderClass(adapterName: string | undefined | null): string {
  const name = (adapterName ?? "").toLowerCase();

  if (name.includes("codex") || name.includes("openai") || name.startsWith("gpt-")) {
    return "border-l-[rgb(var(--primary))]";
  }

  if (name.includes("claude") || name.includes("anthropic")) {
    return "border-l-[rgb(var(--thinking))]";
  }

  return "border-l-[rgb(var(--muted-foreground))]";
}
