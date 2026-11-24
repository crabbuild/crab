import { useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";

type Deleted = {
  id: string;
  owner: string;
  name: string;
  deletion_id: string;
  deleted_at_ms: number;
  expires_at_ms: number;
  can_restore: boolean;
  forks: number;
};
type Restored = Pick<Repository, "id" | "owner" | "name">;

const date = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });

export function DeletedRepositories({
  id,
  csrf,
}: {
  id: string;
  csrf: string;
}) {
  const path = `/api/organizations/${id}/deleted-repositories`;
  const [before, setBefore] = useState<string>();
  const [restored, setRestored] = useState<Restored>();
  const rows = useRequest<{ items: Deleted[]; next: string | null }>(
    before ? `${path}?${new URLSearchParams({ before })}` : path,
  );
  return (
    <div className="grid gap-4">
      <h2 className="text-lg font-semibold">Deleted repositories</h2>
      {restored && (
        <p role="status" className="text-sm wrap-anywhere">
          <Link href={repoHref(restored)} className="font-medium underline">
            {restored.name}
          </Link>{" "}
          was restored.
        </p>
      )}
      <Result state={rows} timingLabel="Deleted repositories">
        {({ items, next }) => (
          <>
            {items.length ? (
              <ul className="divide-y rounded-lg border">
                {items.map((row) => (
                  <li
                    key={row.id}
                    className="flex min-w-0 flex-wrap items-center justify-between gap-3 p-4"
                  >
                    <div className="min-w-0 wrap-anywhere">
                      <p className="font-medium">{row.name}</p>
                      <p className="text-sm text-muted-foreground">
                        Deleted {date.format(row.deleted_at_ms)}.{" "}
                        {row.can_restore
                          ? `Available to restore until ${date.format(row.expires_at_ms)}.`
                          : "A fork cannot be restored separately."}
                      </p>
                    </div>
                    {row.can_restore && (
                      <RestoreAction
                        row={row}
                        path={`${path}/${row.id}`}
                        csrf={csrf}
                        onRestored={(record) => {
                          setRestored(record);
                          rows.retry();
                        }}
                      />
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <p role="status" className="text-sm text-muted-foreground">
                No recently deleted repositories on this page.
              </p>
            )}
            {(before || next) && (
              <div className="flex flex-wrap gap-2">
                {before && (
                  <Button
                    variant="outline"
                    onClick={() => setBefore(undefined)}
                  >
                    First page
                  </Button>
                )}
                {next && (
                  <Button variant="outline" onClick={() => setBefore(next)}>
                    Next page
                  </Button>
                )}
              </div>
            )}
          </>
        )}
      </Result>
    </div>
  );
}

function RestoreAction({
  row,
  path,
  csrf,
  onRestored,
}: {
  row: Deleted;
  path: string;
  csrf: string;
  onRestored: (record: Restored) => void;
}) {
  const [open, setOpen] = useState(false);
  const mutation = useMutation(csrf, null);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" aria-label={`Restore ${row.name}`}>
          Restore
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle className="wrap-anywhere">
            Restore {row.name}
          </DialogTitle>
          <DialogDescription>
            Restore access to code and collaboration history.{" "}
            {row.forks > 0 &&
              `The repository’s ${row.forks} private ${row.forks === 1 ? "fork returns" : "forks return"} with it. `}
            Team grants need to be added again.
          </DialogDescription>
        </DialogHeader>
        {mutation.error && (
          <p role="alert" className="text-destructive">
            {mutation.error}
          </p>
        )}
        <Button
          disabled={mutation.pending}
          onClick={async () => {
            const result = await mutation.run<Restored>(path, "PUT", {
              deletion_id: row.deletion_id,
            });
            if (result) {
              setOpen(false);
              onRestored(result);
            }
          }}
        >
          {mutation.pending ? "Restoring…" : "Restore repository"}
        </Button>
      </DialogContent>
    </Dialog>
  );
}
