import { useState } from "react";
import { LockKeyholeIcon, UsersIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { FieldSelect } from "@/components/ui/field-select";
import { TeamFields } from "./form";
import { TeamPages } from "./members";
import { TeamDetail } from "./team";
import {
  HierarchyPicker,
  HierarchyRequests,
  type TeamChoice,
} from "./hierarchy";
import { useTeamRead } from "./read";
import type { TeamListing, TeamPolicy } from "@/lib/contracts";
import { teamHref, type Draft } from "./model";
import type { Commands } from "./commands";

export function Teams({
  id,
  actor,
  owner,
  url,
  csrf,
  commands,
}: {
  id: string;
  actor: string;
  owner: boolean;
  url: URL;
  csrf: string;
  commands: Commands;
}) {
  const team = url.searchParams.get("team");
  if (
    team !== null &&
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(
      team,
    )
  )
    return (
      <div className="notice error" role="alert">
        <p>Team not found.</p>
        <Link href={teamHref(id)}>Back to teams</Link>
      </div>
    );
  return team ? (
    <TeamDetail
      key={team}
      organization={id}
      actor={actor}
      team={team}
      url={url}
      csrf={csrf}
      owner={owner}
      commands={commands}
    />
  ) : url.searchParams.get("view") === "requests" ? (
    <HierarchyRequests
      organization={id}
      actor={actor}
      request={url.searchParams.get("request")}
      commands={commands}
    />
  ) : (
    <TeamList id={id} actor={actor} owner={owner} commands={commands} />
  );
}
function TeamList({
  id,
  actor,
  owner,
  commands,
}: {
  id: string;
  actor: string;
  owner: boolean;
  commands: Commands;
}) {
  const [query, setQuery] = useState("");
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState<string>();
  const [creating, setCreating] = useState(false);
  const [parent, setParent] = useState<TeamChoice | null>(null);
  const [draft, setDraft] = useState<Draft>({
    name: "",
    description: "",
    visibility: "secret",
    version: 0,
  });
  const root = `/api/organizations/${id}/teams`;
  const params = new URLSearchParams({ search });
  if (after) params.set("after", after);
  const listing = useTeamRead<TeamListing>(
    `${root}?${params}`,
    commands.generation,
  );
  function refresh() {
    setAfter(undefined);
    listing.retry();
  }
  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-lg font-semibold">Organization teams</h2>
        <div className="flex flex-wrap items-center gap-2">
          <Button asChild variant="outline">
            <Link href={teamHref(id, undefined, "requests")}>
              Team requests
            </Link>
          </Button>
          <Button variant="outline" onClick={refresh}>
            Refresh teams
          </Button>
          {listing.data?.can_create && (
            <Button
              disabled={commands.locked}
              onClick={() => setCreating((value) => !value)}
              aria-expanded={creating}
              aria-controls={creating ? "new-team" : undefined}
            >
              {creating ? "Close new team" : "New team"}
            </Button>
          )}
        </div>
      </div>
      <form
        role="search"
        aria-label="Team search"
        className="flex flex-wrap items-center gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          setAfter(undefined);
          if (query.trim() === search && !after) listing.retry();
          setSearch(query.trim());
        }}
      >
        <Input
          id="team-search"
          className="min-w-36 max-w-md flex-1"
          aria-label="Find a team"
          type="search"
          value={query}
          maxLength={100}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search teams…"
        />
        <Button variant="outline" type="submit">
          Search
        </Button>
        {search && (
          <Button
            variant="ghost"
            type="button"
            onClick={() => {
              setQuery("");
              setSearch("");
              setAfter(undefined);
            }}
          >
            Clear search
          </Button>
        )}
      </form>
      {creating && (
        <form
          id="new-team"
          className="flex max-w-2xl flex-col gap-5 rounded-lg border p-5"
          onSubmit={(event) => {
            event.preventDefault();
            const { name, description, visibility } = draft;
            void commands
              .submit(
                id,
                actor,
                {
                  kind: "create",
                  name,
                  description,
                  visibility,
                  parent: visibility === "closed" ? parent?.id : undefined,
                },
                "Create team",
              )
              .then((value) => {
                if (value?.outcome === "applied") {
                  setCreating(false);
                  setParent(null);
                  setDraft({
                    name: "",
                    description: "",
                    visibility: "secret",
                    version: 0,
                  });
                }
              });
          }}
        >
          <h2 className="text-lg font-semibold">Create a new team</h2>
          <TeamFields
            draft={draft}
            onChange={setDraft}
            disabled={commands.locked}
          />
          {draft.visibility === "closed" && (
            <HierarchyPicker
              organization={id}
              value={parent}
              generation={commands.generation}
              disabled={commands.locked}
              onChange={setParent}
            />
          )}
          <Button
            className="w-fit"
            type="submit"
            disabled={commands.locked || !listing.data?.can_create}
          >
            Create team
          </Button>
        </form>
      )}
      <Result state={listing} timingLabel="Organization teams">
        {(data) => (
          <>
            <div className="overflow-hidden rounded-lg border">
              {data.items.length ? (
                <ul className="divide-y">
                  {data.items.map((item) => (
                    <li
                      key={item.team.id}
                      className="flex min-w-0 items-start gap-4 p-5"
                    >
                      <UsersIcon
                        className="mt-0.5 size-5 shrink-0 text-muted-foreground"
                        aria-hidden="true"
                      />
                      <div className="min-w-0 flex-1">
                        <div className="flex flex-wrap items-center gap-2">
                          <Link
                            className="min-w-0 wrap-anywhere text-base font-semibold"
                            href={teamHref(id, item.team.id)}
                          >
                            {item.team.name}
                          </Link>
                          {item.team.visibility === "secret" && (
                            <Badge variant="outline">
                              <LockKeyholeIcon data-icon="inline-start" />
                              Secret
                            </Badge>
                          )}
                        </div>
                        {item.team.description && (
                          <p
                            dir="auto"
                            className="mt-1 whitespace-pre-wrap wrap-anywhere text-sm text-muted-foreground"
                          >
                            {item.team.description}
                          </p>
                        )}
                        <p className="mt-2 text-sm text-muted-foreground">
                          {item.members.toLocaleString()}{" "}
                          {item.members === 1
                            ? "direct member"
                            : "direct members"}
                          {item.role
                            ? ` · ${item.role === "maintainer" ? "Maintainer" : "Member"}`
                            : ""}
                        </p>
                      </div>
                    </li>
                  ))}
                </ul>
              ) : (
                <p
                  role="status"
                  className="p-6 text-center text-sm text-muted-foreground"
                >
                  {search
                    ? "No teams match your search."
                    : "No teams to show. Teams help organize people who collaborate."}
                </p>
              )}
            </div>
            <TeamPages after={after} next={data.next} onPage={setAfter} />
          </>
        )}
      </Result>
      {owner && <CreationPolicy id={id} actor={actor} commands={commands} />}
    </div>
  );
}
function CreationPolicy({
  id,
  actor,
  commands,
}: {
  id: string;
  actor: string;
  commands: Commands;
}) {
  const state = useTeamRead<TeamPolicy>(
    `/api/organizations/${id}/teams/policy`,
    commands.generation,
  );
  return (
    <Result state={state} timingLabel="Team creation permission">
      {(policy) => (
        <section className="mt-4 flex flex-wrap items-center justify-between gap-4 rounded-lg border p-5">
          <div>
            <h2 className="font-semibold">Team creation</h2>
            <p className="mt-1 text-sm text-muted-foreground">
              Choose who can create teams in this organization.
            </p>
          </div>
          <Field className="w-auto">
            <FieldLabel className="sr-only" htmlFor="team-creation">
              Who can create teams
            </FieldLabel>
            <FieldSelect
              id="team-creation"
              label="Who can create teams"
              value={policy.member_creation ? "members" : "owners"}
              disabled={commands.locked}
              onValueChange={(value) =>
                void commands.submit(
                  id,
                  actor,
                  {
                    kind: "policy",
                    version: policy.version,
                    member_creation: value === "members",
                  },
                  "Update team creation permission",
                )
              }
              options={{
                members: "All organization members",
                owners: "Organization owners only",
              }}
            />
          </Field>
        </section>
      )}
    </Result>
  );
}
