import { useState } from "react";
import { PlusIcon } from "lucide-react";
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
import { useRequest } from "@/lib/api";
import type { Team, TeamListing } from "@/lib/contracts";

export function TeamPicker({
  organization,
  selected,
  disabled,
  onSelect,
}: {
  organization: string;
  selected: ReadonlySet<string>;
  disabled: boolean;
  onSelect: (team: Team) => void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button disabled={disabled}>
          <PlusIcon data-icon="inline-start" /> Add team
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Add a team</DialogTitle>
          <DialogDescription>
            Choose a team from the owning organization.
          </DialogDescription>
        </DialogHeader>
        {open && (
          <TeamChoices
            organization={organization}
            selected={selected}
            onSelect={(team) => {
              onSelect(team);
              setOpen(false);
            }}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}

function TeamChoices({
  organization,
  selected,
  onSelect,
}: {
  organization: string;
  selected: ReadonlySet<string>;
  onSelect: (team: Team) => void;
}) {
  const [query, setQuery] = useState("");
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState<string>();
  const params = new URLSearchParams({ search });
  if (after) params.set("after", after);
  const listing = useRequest<TeamListing>(
    `/api/organizations/${encodeURIComponent(organization)}/teams?${params}`,
  );
  return (
    <>
      <form
        className="flex items-end gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          const term = query.trim();
          if (!after && search === term) listing.retry();
          setAfter(undefined);
          setSearch(term);
        }}
      >
        <Field className="min-w-0">
          <FieldLabel htmlFor="grant-team-search">Find a team</FieldLabel>
          <Input
            id="grant-team-search"
            value={query}
            maxLength={100}
            onChange={(event) => setQuery(event.target.value)}
          />
        </Field>
        <Button type="submit" variant="outline">
          Search
        </Button>
      </form>
      {listing.error && after && (
        <Button
          variant="outline"
          className="w-fit"
          onClick={() => setAfter(undefined)}
        >
          First page
        </Button>
      )}
      <Result
        state={listing}
        timingLabel="Teams available for repository grants"
      >
        {({ items, next }) => (
          <div className="flex flex-col gap-3">
            {items.length ? (
              <ul
                aria-label="Organization teams"
                className="m-0 max-h-[min(20rem,40dvh)] list-none divide-y overflow-auto rounded-lg border p-0"
              >
                {items.map(({ team }) => (
                  <li
                    key={team.id}
                    className="flex items-center justify-between gap-2 p-2"
                  >
                    <span className="min-w-0 font-medium wrap-anywhere">
                      {team.name}
                    </span>
                    {selected.has(team.id) ? (
                      <span className="shrink-0 text-muted-foreground">
                        Added
                      </span>
                    ) : (
                      <Button
                        size="sm"
                        variant="outline"
                        aria-label={`Add ${team.name}`}
                        onClick={() => onSelect(team)}
                      >
                        Add
                      </Button>
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-muted-foreground">
                {search
                  ? "No teams match your search."
                  : "This organization has no teams."}
              </p>
            )}
            {(next || after) && (
              <div className="flex gap-2">
                {after && (
                  <Button variant="outline" onClick={() => setAfter(undefined)}>
                    First page
                  </Button>
                )}
                {next && (
                  <Button variant="outline" onClick={() => setAfter(next)}>
                    Next page
                  </Button>
                )}
              </div>
            )}
          </div>
        )}
      </Result>
    </>
  );
}
