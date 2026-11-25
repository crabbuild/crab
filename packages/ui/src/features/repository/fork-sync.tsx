import { SyncIcon } from "@primer/octicons-react";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Spinner } from "@/components/ui/spinner";
import {
  endpoint,
  updateForkBranch,
  useRequest,
  type Repository,
} from "@/lib/api";

const statusText = {
  behind: "This branch is behind upstream and can be updated.",
  up_to_date: "This branch is up to date with upstream.",
  ahead: "This branch already includes the upstream changes.",
  diverged:
    "Both branches have new commits. Merge the upstream changes in Git to keep your work.",
  upstream_missing: "There is no matching branch upstream.",
  fork_missing: "You can copy this upstream branch from the Branches page.",
};

interface Comparison {
  state: keyof typeof statusText;
  branch: string;
  fork_oid: string | null;
  upstream_oid: string | null;
}

export function ForkSync({
  repo,
  branch,
  csrf,
  onUpdated,
}: {
  repo: Repository;
  branch: string;
  csrf: string;
  onUpdated: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [updating, setUpdating] = useState(false);
  const [error, setError] = useState<string>();
  const comparison = useRequest<Comparison>(
    open ? endpoint(repo, "fork-sync", { branch }) : null,
  );

  async function update() {
    const preview = comparison.data;
    if (
      !preview?.fork_oid ||
      !preview.upstream_oid ||
      preview.state !== "behind" ||
      updating
    )
      return;
    setUpdating(true);
    setError(undefined);
    try {
      await updateForkBranch(
        repo,
        {
          branch: preview.branch,
          fork_oid: preview.fork_oid,
          upstream_oid: preview.upstream_oid,
        },
        csrf,
      );
      setOpen(false);
      onUpdated();
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "Could not update the branch",
      );
    } finally {
      setUpdating(false);
    }
  }

  function refresh() {
    setError(undefined);
    comparison.retry();
  }

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        if (next && !updating) setError(undefined);
        setOpen(next);
      }}
    >
      <PopoverTrigger asChild>
        <Button variant="outline" size="sm">
          <SyncIcon data-icon="inline-start" /> Sync fork
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" aria-label="Sync fork">
        <PopoverTitle className="break-all">
          {branch.slice("refs/heads/".length)}
        </PopoverTitle>
        {comparison.loading && (
          <p className="flex items-center gap-2" role="status">
            <Spinner /> Checking upstream…
          </p>
        )}
        {comparison.data && (
          <>
            <p className="text-muted-foreground">
              {statusText[comparison.data.state]}
            </p>
            {comparison.data.state === "behind" && (
              <>
                <p className="text-xs text-muted-foreground">
                  {comparison.data.fork_oid?.slice(0, 7)} →{" "}
                  {comparison.data.upstream_oid?.slice(0, 7)}
                </p>
                <Button
                  size="sm"
                  disabled={updating}
                  onClick={() => void update()}
                >
                  {updating ? "Updating…" : "Update branch"}
                </Button>
              </>
            )}
          </>
        )}
        {(error || comparison.error) && (
          <>
            <p role="alert">{error ?? comparison.error}</p>
            <Button
              variant="outline"
              size="sm"
              disabled={updating}
              onClick={refresh}
            >
              Refresh comparison
            </Button>
          </>
        )}
      </PopoverContent>
    </Popover>
  );
}
