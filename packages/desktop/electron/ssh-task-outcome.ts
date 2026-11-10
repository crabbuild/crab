/** Separates confirmed task mutations from cleanup that needs user attention. */
export function confirmRemoteTaskOperation(value: unknown, operation: "apply" | "discard"): string | undefined {
  const receipt = value && typeof value === "object" ? value as Record<string, unknown> : {};
  const result = operation === "apply" ? "applied" : "discarded";
  if (receipt[result] !== true) throw new Error(`Remote ${operation} was not confirmed`);
  if (receipt.worktree_removed === true && receipt.branch_deleted === true && !receipt.cleanup_error) return;
  const detail = typeof receipt.cleanup_error === "string" ? `: ${receipt.cleanup_error}` : ". Inspect the remote worktree and task branch before cleanup.";
  return `Task ${result}; remote cleanup needs attention${detail}`;
}

