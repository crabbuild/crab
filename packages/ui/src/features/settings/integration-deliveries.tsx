import { useState } from "react";
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
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import type { Connection } from "./integrations";

type Delivery = {
  id: string;
  received_at_ms: number;
  oid: string;
  run_number: number;
  attempt: number;
  status: string;
  conclusion: string | null;
  details_url: string | null;
  updated_check: boolean;
};

const receivedAt = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "medium",
});

export function IntegrationDeliveries({
  repo,
  connection,
}: {
  repo: Repository;
  connection: Connection;
}) {
  const [open, setOpen] = useState(false);
  const path = endpoint(
    repo,
    `settings/integrations/${encodeURIComponent(connection.id)}/deliveries`,
  );
  const request = useRequest<Delivery[]>(open ? path : null, repo.id);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button
          size="sm"
          variant="outline"
          aria-label={`Recent deliveries for ${connection.name}`}
        >
          Recent deliveries
        </Button>
      </DialogTrigger>
      <DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-xl">
        <DialogHeader className="pr-6">
          <DialogTitle>Recent deliveries</DialogTitle>
          <DialogDescription>
            The latest accepted GitHub Actions reports for {connection.name}.
          </DialogDescription>
        </DialogHeader>
        {!request.error && (
          <Button
            size="sm"
            variant="outline"
            className="w-fit"
            disabled={request.loading}
            onClick={request.retry}
          >
            Refresh deliveries
          </Button>
        )}
        <Result state={request} timingLabel="GitHub Actions deliveries">
          {(items) =>
            items.length ? (
              <ul className="divide-y rounded-md border">
                {items.map((item) => (
                  <li key={item.id} className="grid gap-2 p-3">
                    <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
                      <strong>
                        Run #{item.run_number} · Attempt {item.attempt}
                      </strong>
                      <time
                        className="text-xs text-muted-foreground"
                        dateTime={new Date(item.received_at_ms).toISOString()}
                      >
                        {receivedAt.format(item.received_at_ms)}
                      </time>
                    </div>
                    <p className="flex flex-wrap gap-x-2 text-xs text-muted-foreground">
                      <span className="capitalize">
                        {(item.conclusion ?? item.status).replaceAll("_", " ")}
                      </span>
                      <span>
                        {item.updated_check
                          ? "Check updated"
                          : "Check unchanged"}
                      </span>
                    </p>
                    <dl className="grid grid-cols-[auto_1fr] gap-x-2 gap-y-1 text-xs">
                      <dt>Commit</dt>
                      <dd className="min-w-0">
                        <Link
                          href={repoHref(repo, {
                            view: "commit",
                            rev: item.oid,
                            repository_id: repo.id,
                          })}
                          className="break-all font-mono underline"
                        >
                          {item.oid}
                        </Link>
                      </dd>
                      <dt>Delivery ID</dt>
                      <dd className="break-all font-mono">{item.id}</dd>
                    </dl>
                    {item.details_url && (
                      <a href={item.details_url} className="w-fit underline">
                        Open GitHub Actions run
                      </a>
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-muted-foreground">
                No accepted deliveries yet.
              </p>
            )
          }
        </Result>
      </DialogContent>
    </Dialog>
  );
}
