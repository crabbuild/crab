import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import type {
  OrganizationMembers,
  RepositoryAssignee,
  TeamListing,
} from "@/lib/contracts";
import type { BypassOptions } from "./ruleset-bypass";

export type ReviewDismissalActor = { kind: "user" | "team"; id: string };
type Choice = ReviewDismissalActor & { name: string };
const key = ({ kind, id }: ReviewDismissalActor) => `${kind}:${id}`;

export function ReviewDismissalEditor({
  actors,
  onChange,
  organization,
  repo,
  teams,
}: {
  actors: ReviewDismissalActor[];
  onChange: (actors: ReviewDismissalActor[]) => void;
  organization?: string;
  repo?: Repository;
  teams?: BypassOptions["teams"];
}) {
  const [search, setSearch] = useState("");
  const [names, setNames] = useState<Record<string, string>>({});
  const root = organization
    ? `/api/organizations/${encodeURIComponent(organization)}`
    : null;
  const people = useRequest<OrganizationMembers>(
    root
      ? `${root}/members?${new URLSearchParams({ q: search, limit: "50" })}`
      : null,
  );
  const collaborators = useRequest<{ items: RepositoryAssignee[] }>(
    repo ? endpoint(repo, "assignees") : null,
  );
  const orgTeams = useRequest<TeamListing>(
    root
      ? `${root}/teams?${new URLSearchParams({ search, limit: "50" })}`
      : null,
  );
  const candidates: Choice[] = [
    ...(people.data?.items ?? []).map(({ actor, profile }) => ({
      kind: "user" as const,
      id: actor,
      name: profile?.handle ?? `Member ${actor.slice(0, 8)}`,
    })),
    ...(collaborators.data?.items ?? []).map(({ id, name, profile }) => ({
      kind: "user" as const,
      id,
      name: profile?.handle ?? name,
    })),
    ...(
      orgTeams.data?.items
        .filter(({ team }) => team.visibility === "closed")
        .map(({ team }) => team) ??
      teams ??
      []
    ).map(({ id, name }) => ({
      kind: "team" as const,
      id,
      name,
    })),
  ];
  const selected = new Set(actors.map(key));
  const showing = candidates.filter(
    (actor) =>
      root || actor.name.toLowerCase().includes(search.trim().toLowerCase()),
  );
  return (
    <fieldset className="grid gap-2">
      <legend className="font-medium">Who can dismiss reviews</legend>
      <p className="text-sm text-muted-foreground">
        Selected people and team members also need repository write access.
        {actors.length === 0 && " An empty list prevents manual dismissal."}
      </p>
      {actors.map((actor) => {
        const id = key(actor);
        const name =
          names[id] ??
          candidates.find((entry) => key(entry) === id)?.name ??
          `${actor.kind === "team" ? "Team" : "Member"} ${actor.id.slice(0, 8)}`;
        return (
          <div key={id} className="flex flex-wrap items-center gap-2">
            <span className="min-w-36 break-all text-sm">{name}</span>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              aria-label={`Remove ${name} from review dismissal`}
              onClick={() =>
                onChange(actors.filter((entry) => key(entry) !== id))
              }
            >
              Remove
            </Button>
          </div>
        );
      })}
      <label className="grid gap-1">
        Find people or teams
        <Input
          type="search"
          maxLength={100}
          value={search}
          onChange={(event) => setSearch(event.target.value)}
        />
      </label>
      {(people.error || collaborators.error || orgTeams.error) && (
        <p role="status">Some people or teams could not be loaded.</p>
      )}
      <label className="grid gap-1">
        Allow review dismissal for
        <select
          className="rounded-md border bg-background px-3 py-2 text-sm"
          disabled={actors.length >= 64}
          value=""
          onChange={(event) => {
            const actor = showing.find(
              (entry) => key(entry) === event.target.value,
            );
            if (actor) {
              const { kind, id, name } = actor;
              setNames((current) => ({ ...current, [key(actor)]: name }));
              onChange([...actors, { kind, id }]);
            }
          }}
        >
          <option value="" disabled>
            Choose a person or team
          </option>
          {showing.map((actor) => (
            <option
              key={key(actor)}
              value={key(actor)}
              disabled={selected.has(key(actor))}
            >
              {actor.name}
              {actor.kind === "team" ? " (team)" : ""}
            </option>
          ))}
        </select>
      </label>
    </fieldset>
  );
}
