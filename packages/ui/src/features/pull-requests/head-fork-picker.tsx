import { useEffect, useState } from "react";
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
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";

export type HeadFork = Pick<Repository, "id" | "owner" | "name"> & {
  owned: boolean;
};

function forkLabel(fork: HeadFork) {
  return fork.owned ? `${fork.name} (your fork)` : `${fork.owner}/${fork.name}`;
}

export function HeadForkPicker({
  repo,
  selected,
  disabled,
  onSelect,
}: {
  repo: Repository;
  selected: HeadFork | undefined;
  disabled: boolean;
  onSelect: (fork: HeadFork | undefined) => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button type="button" size="sm" variant="outline" disabled={disabled}>
          {selected ? forkLabel(selected) : "Compare across forks"}
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Head repository</DialogTitle>
          <DialogDescription>
            Choose where to compare a branch with {repo.name}.
          </DialogDescription>
        </DialogHeader>
        {open && (
          <ForkChoices
            repo={repo}
            selected={selected}
            onSelect={(fork) => {
              onSelect(fork);
              setOpen(false);
            }}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}

function ForkChoices({
  repo,
  selected,
  onSelect,
}: {
  repo: Repository;
  selected: HeadFork | undefined;
  onSelect: (fork: HeadFork | undefined) => void;
}) {
  const [filter, setFilter] = useState("");
  const [query, setQuery] = useState("");
  useEffect(() => {
    const timer = window.setTimeout(() => setQuery(filter), 200);
    return () => window.clearTimeout(timer);
  }, [filter]);
  const results = useRequest<{ items: HeadFork[]; truncated: boolean }>(
    filter === query ? endpoint(repo, "pulls/head-forks", { q: query }) : null,
    repo.id,
  );

  return (
    <>
      <Field>
        <FieldLabel htmlFor="head-fork-search">Search forks</FieldLabel>
        <Input
          id="head-fork-search"
          placeholder="Owner or repository"
          value={filter}
          maxLength={128}
          onChange={(event) => setFilter(event.target.value)}
        />
      </Field>
      {selected && (
        <Button
          type="button"
          variant="outline"
          onClick={() => onSelect(undefined)}
        >
          Use upstream repository
        </Button>
      )}
      <Result state={results} timingLabel="Head forks">
        {({ items, truncated }) =>
          items.length ? (
            <>
              <ul
                className="flex max-h-64 flex-col gap-1 overflow-auto"
                aria-label="Writable forks"
              >
                {items.map((fork) => (
                  <li key={fork.id}>
                    <Button
                      type="button"
                      variant="ghost"
                      className="h-auto w-full justify-start text-left whitespace-normal break-all"
                      aria-pressed={selected?.id === fork.id}
                      onClick={() => onSelect(fork)}
                    >
                      {forkLabel(fork)}
                    </Button>
                  </li>
                ))}
              </ul>
              {truncated && (
                <p role="status">Refine your search to see more forks.</p>
              )}
            </>
          ) : (
            <p role="status">
              {query
                ? "No writable forks match your search."
                : "You don’t have Write access to any forks of this repository."}
            </p>
          )
        }
      </Result>
    </>
  );
}
