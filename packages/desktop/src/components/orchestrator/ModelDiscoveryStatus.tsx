import type { useAgentModels } from "./model-catalog";

export type ModelDiscoveryControls = Pick<ReturnType<typeof useAgentModels>, "status" | "loading" | "cancel" | "retry">;

export function ModelDiscoveryStatus({ discovery }: { discovery?: ModelDiscoveryControls }) {
  if (!discovery?.status) return null;
  return <div className="mb-2 flex items-center gap-2 text-xs text-gx-muted">
    <p role="status">{discovery.status}</p>
    <button type="button" aria-label={discovery.loading ? "Cancel model discovery" : "Retry model discovery"} onClick={discovery.loading ? discovery.cancel : discovery.retry} className="underline hover:text-gx-text">
      {discovery.loading ? "Cancel" : "Retry"}
    </button>
  </div>;
}
