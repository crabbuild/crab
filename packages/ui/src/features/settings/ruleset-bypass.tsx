import { FieldSelect } from "@/components/ui/field-select";
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
            <FieldSelect
              aria-label={`Bypass mode for ${label}`}
              className="w-full max-w-xs"
              value={grant.mode}
              onValueChange={(nextValue) =>
                onChange(
                  grants.map((item) =>
                    item === grant
                      ? {
                          ...item,
                          mode:
                            nextValue === "pull_request"
                              ? "pull_request"
                              : "always",
                        }
                      : item,
                  ),
                )
              }
              options={[
                { value: "always", label: "Always" },
                ...(branch || grant.mode === "pull_request"
                  ? [
                      {
                        value: "pull_request",
                        label: "Pull requests only",
                        disabled: !branch,
                      },
                    ]
                  : []),
              ]}
            />
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
        <FieldSelect
          className="w-full max-w-xs"
          value=""
          onValueChange={(nextValue) => {
            const added = actors.find(
              ({ actor }) => actorKey(actor) === nextValue,
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
          options={[
            { value: "", label: "Select a role or team", disabled: true },
            ...actors.map(({ actor, label }) => {
              const key = actorKey(actor);
              return {
                value: key,
                label: label,
                disabled: grants.some((grant) => actorKey(grant.actor) === key),
              };
            }),
          ]}
        />
      </label>
    </fieldset>
  );
}
