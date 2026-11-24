import { useEffect, useRef, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
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
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import type { Issue } from "./model";

type Candidate = Pick<Repository, "id" | "owner" | "name">;

export function IssueMove({
  repo,
  issue,
  csrf,
  onAccepted,
}: {
  repo: Repository;
  issue: Issue;
  csrf: string;
  onAccepted: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState("");
  const url = `issues/${issue.number}/transfer`;
  const candidates = useRequest<Candidate[]>(
    open ? endpoint(repo, url, { q: query }) : null,
    repo.id,
  );
  const mutation = useMutation(csrf, repo);
  const attempt = useRef<{ key: string; id: string }>(undefined);
  const needsReload =
    mutation.conflict ||
    mutation.code === "request_conflict" ||
    mutation.code === "issue_transfer_expired";
  useEffect(() => {
    const timer = window.setTimeout(() => setQuery(search.trim()), 180);
    return () => window.clearTimeout(timer);
  }, [search]);

  const destination = candidates.data?.find(
    (candidate) => candidate.id === selected,
  );
  return (
    <Dialog
      open={open}
      onOpenChange={(value) => {
        if (!mutation.pending) setOpen(value);
      }}
    >
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">
          Transfer issue
        </Button>
      </DialogTrigger>
      <DialogContent showCloseButton={!mutation.pending}>
        <DialogHeader>
          <DialogTitle>Transfer issue #{issue.number}</DialogTitle>
          <DialogDescription>
            The original issue link will lead to the new location once the
            transfer finishes.
          </DialogDescription>
        </DialogHeader>
        <form
          className="grid gap-3"
          onSubmit={async (event) => {
            event.preventDefault();
            if (!destination || mutation.pending || needsReload) return;
            const key = JSON.stringify([issue.version, destination.id]);
            if (attempt.current?.key !== key)
              attempt.current = { key, id: crypto.randomUUID() };
            const receipt = await mutation.run<{ accepted: boolean }>(
              endpoint(repo, url),
              "POST",
              {
                request_id: attempt.current.id,
                version: issue.version,
                destination: destination.id,
              },
            );
            if (receipt?.accepted) {
              setOpen(false);
              onAccepted();
            }
          }}
        >
          <label htmlFor="issue-transfer-search">
            Find a destination repository
          </label>
          <Input
            id="issue-transfer-search"
            value={search}
            maxLength={100}
            disabled={mutation.pending}
            onChange={(event) => {
              setSearch(event.target.value);
              setSelected("");
            }}
          />
          <Result state={candidates}>
            {(items) =>
              items.length ? (
                <select
                  aria-label="Destination repository"
                  className="rounded-md border bg-background px-3 py-2"
                  value={destination?.id ?? ""}
                  disabled={mutation.pending}
                  onChange={(event) => {
                    setSelected(event.target.value);
                    mutation.reset();
                  }}
                >
                  <option value="">Choose a repository</option>
                  {items.map((item) => (
                    <option key={item.id} value={item.id}>
                      {item.owner}/{item.name}
                    </option>
                  ))}
                </select>
              ) : (
                <p className="muted">No matching repositories.</p>
              )
            }
          </Result>
          {mutation.error && (
            <p className="notice error" role="alert">
              {mutation.error}
            </p>
          )}
          {needsReload && (
            <Button
              type="button"
              variant="outline"
              onClick={() => {
                attempt.current = undefined;
                setOpen(false);
                onAccepted();
              }}
            >
              Reload issue
            </Button>
          )}
          <Button
            type="submit"
            disabled={!destination || mutation.pending || needsReload}
          >
            {mutation.pending ? "Transferring…" : "Transfer issue"}
          </Button>
        </form>
      </DialogContent>
    </Dialog>
  );
}
