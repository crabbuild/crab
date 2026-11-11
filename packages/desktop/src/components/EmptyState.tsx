import crabLogo from "../assets/crab.svg";

export function EmptyState({ onConnectSsh }: { onConnectSsh: () => void }) {
  return (
    <div className="flex h-full w-full items-center justify-center p-10">
      <div className="max-w-md text-center">
        <div className="mx-auto mb-4 flex h-14 w-14 items-center justify-center rounded-full border border-gx-border bg-gx-surface">
          <span
            aria-hidden="true"
            className="h-10 w-10 bg-gx-text"
            style={{ mask: `url("${crabLogo}") center / contain no-repeat` }}
          />
        </div>
        <h1 className="mb-2 text-xl font-semibold">Crab</h1>
        <p className="text-sm text-gx-muted">
          Open a local Crab-tracked repository or clone from{" "}
          <code className="kbd">crab://</code>,{" "}
          <code className="kbd">s3://</code>,{" "}
          <code className="kbd">gs://</code>, or{" "}
          <code className="kbd">az://</code> using the sidebar.
        </p>
        <button type="button" className="btn mt-5 text-sm" onClick={onConnectSsh}>
          SSH Connections
        </button>
      </div>
    </div>
  );
}
