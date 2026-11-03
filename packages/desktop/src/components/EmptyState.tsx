import { GitBranchPlus } from "lucide-react";

export function EmptyState() {
  return (
    <div className="flex h-full w-full items-center justify-center p-10">
      <div className="max-w-md text-center">
        <div className="mx-auto mb-4 flex h-14 w-14 items-center justify-center rounded-full border border-gx-border bg-gx-surface">
          <GitBranchPlus size={24} className="text-gx-accent" />
        </div>
        <h1 className="mb-2 text-xl font-semibold">Crab</h1>
        <p className="text-sm text-gx-muted">
          Open a local Crab-tracked repository or clone from{" "}
          <code className="kbd">crab://</code>,{" "}
          <code className="kbd">s3://</code>,{" "}
          <code className="kbd">gs://</code>, or{" "}
          <code className="kbd">az://</code> using the sidebar.
        </p>
      </div>
    </div>
  );
}
