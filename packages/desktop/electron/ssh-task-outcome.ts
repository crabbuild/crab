/** Separates confirmed task mutations from cleanup that needs user attention. */
export function confirmRemoteTaskOperation(value: unknown, operation: "apply" | "discard"): string | undefined {
  const receipt = value && typeof value === "object" ? value as Record<string, unknown> : {};
  const result = operation === "apply" ? "applied" : "discarded";
  if (receipt[result] !== true) throw new Error(`Remote ${operation} was not confirmed`);
  const recovery = operation === "apply" && typeof receipt.recovery_warning === "string" ? receipt.recovery_warning : undefined;
  if (receipt.worktree_removed === true && receipt.branch_deleted === true && !receipt.cleanup_error) return recovery;
  const detail = typeof receipt.cleanup_error === "string" ? `: ${receipt.cleanup_error}` : ". Inspect the remote worktree and task branch before cleanup.";
  const warning = `Task ${result}; remote cleanup needs attention${detail}`;
  return recovery ? `${warning}\n${recovery}` : warning;
}
