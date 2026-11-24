import { useState } from "react";
import { useRequest } from "@/lib/api";
import type { TeamListing } from "@/lib/contracts";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";

export type BypassActor =
  | { kind: "repository_admin" }
  | { kind: "repository_maintain" }
  | { kind: "repository_write" }
  | { kind: "organization_owner" }
  | { kind: "team"; id: string };

export type BypassGrant = {
  actor: BypassActor;
  mode: "always" | "pull_request";
};

export interface BypassOptions {
  organization_owner: boolean;
  teams: { id: string; name: string }[];
}

const roleOptions = [
  { actor: { kind: "repository_admin" }, label: "Repository administrators" },
  { actor: { kind: "repository_maintain" }, label: "Repository maintainers" },
  { actor: { kind: "repository_write" }, label: "Repository writers" },
  { actor: { kind: "organization_owner" }, label: "Organization owners" },
] satisfies { actor: BypassActor; label: string }[];

function actorKey(actor: BypassActor) {
  return actor.kind === "team" ? actor.id : actor.kind;
}

export function RulesetBypassEditor({
  branch,
  grants,
  options,
  organization,
  onChange,
}: {
  branch: boolean;
  grants: BypassGrant[];
  options?: BypassOptions;
  organization?: string;
  onChange: (grants: BypassGrant[]) => void;
}) {
  const [teamSearch, setTeamSearch] = useState("");
  const teams = useRequest<TeamListing>(
    organization
      ? `/api/organizations/${encodeURIComponent(organization)}/teams?${new URLSearchParams({ search: teamSearch, limit: "50" })}`
      : null,
  );
  const available = organization
    ? teams.data?.items
        .filter(({ team }) => team.visibility === "closed")
        .map(({ team }) => team)
    : options?.teams;
  const actors: { actor: BypassActor; label: string }[] = roleOptions.filter(
    ({ actor }) =>
      actor.kind !== "organization_owner" ||
      organization ||
      options?.organization_owner,
  );
  for (const team of available ?? []) {
    actors.push({ actor: { kind: "team", id: team.id }, label: team.name });
  }
  const selectStyle = "rounded-md border bg-background px-3 py-2 text-sm";
  return (
    <fieldset className="grid gap-2">
      <legend className="font-medium">Bypass access</legend>
      <p className="text-sm text-muted-foreground">
        Bypass grants require repository write access.
      </p>
      {grants.map((grant) => {
        const key = actorKey(grant.actor);
        const label =
          actors.find(({ actor }) => actorKey(actor) === key)?.label ??
          "Unavailable team";
        return (
          <div key={key} className="flex flex-wrap items-center gap-2">
            <span className="min-w-36 break-all text-sm">{label}</span>
            <select
              aria-label={`Bypass mode for ${label}`}
              className={selectStyle}
              value={grant.mode}
              onChange={(event) =>
                onChange(
                  grants.map((item) =>
                    item === grant
                      ? {
                          ...item,
                          mode:
                            event.target.value === "pull_request"
                              ? "pull_request"
                              : "always",
                        }
                      : item,
                  ),
                )
              }
            >
              <option value="always">Always</option>
              {(branch || grant.mode === "pull_request") && (
                <option disabled={!branch} value="pull_request">
                  Pull requests only
                </option>
              )}
            </select>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              aria-label={`Remove bypass for ${label}`}
              onClick={() => onChange(grants.filter((item) => item !== grant))}
            >
              Remove
            </Button>
          </div>
        );
      })}
      {organization && (
        <label className="grid gap-1">
          Search bypass teams
          <Input
            type="search"
            maxLength={100}
            value={teamSearch}
            onChange={(event) => setTeamSearch(event.target.value)}
          />
        </label>
      )}
      {teams.error && (
        <p role="status">Organization teams could not be loaded.</p>
      )}
      <label className="grid gap-1">
        Add bypass actor
        <select
          className={selectStyle}
          value=""
          onChange={(event) => {
            const added = actors.find(
              ({ actor }) => actorKey(actor) === event.target.value,
            );
            if (added)
              onChange([
                ...grants,
                {
                  actor: added.actor,
                  mode: branch ? "pull_request" : "always",
                },
              ]);
          }}
        >
          <option value="" disabled>
            Select a role or team
          </option>
          {actors.map(({ actor, label }) => {
            const key = actorKey(actor);
            return (
              <option
                value={key}
                key={key}
                disabled={grants.some((grant) => actorKey(grant.actor) === key)}
              >
                {label}
              </option>
            );
          })}
        </select>
      </label>
    </fieldset>
  );
}
