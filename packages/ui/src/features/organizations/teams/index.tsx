import { useState } from "react";
import { LockKeyholeIcon, UsersIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { TeamFields } from "./form";
import { TeamPages } from "./members";
import { TeamDetail } from "./team";
import { useTeamRead } from "./read";
import { teamHref, type Draft, type Listing, type Policy } from "./model";
import type { Commands } from "./commands";

export function Teams({
  id,
  actor,
  owner,
  url,
  commands,
}: {
  id: string;
  actor: string;
  owner: boolean;
  url: URL;
  commands: Commands;
}) {
  const team = url.searchParams.get("team");
  return team ? (
    <TeamDetail
      key={team}
      organization={id}
      actor={actor}
      team={team}
      url={url}
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
  const [draft, setDraft] = useState<Draft>({
    name: "",
    description: "",
    visibility: "secret",
    version: 0,
  });
  const root = `/api/organizations/${id}/teams`;
  const params = new URLSearchParams({ search });
  if (after) params.set("after", after);
  const listing = useTeamRead<Listing>(
    `${root}?${params}`,
    commands.generation,
  );
  function refresh() {
    setAfter(undefined);
    listing.retry();
  }
  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <form
          className="flex min-w-0 flex-1 items-end gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            setAfter(undefined);
            setSearch(query);
          }}
        >
          <Field className="max-w-md">
            <FieldLabel htmlFor="team-search">Find a team</FieldLabel>
            <Input
              id="team-search"
              value={query}
              maxLength={100}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Search teams…"
            />
          </Field>
          <Button variant="outline" type="submit">
            Search
          </Button>
        </form>
        <Button variant="outline" onClick={refresh}>
          Refresh teams
        </Button>
        {listing.data?.can_create && (
          <Button
            disabled={commands.locked}
            onClick={() => setCreating((value) => !value)}
          >
            {creating ? "Close new team" : "New team"}
          </Button>
        )}
      </div>
      {creating && (
        <form
          className="flex max-w-2xl flex-col gap-5 rounded-lg border p-5"
          onSubmit={(event) => {
            event.preventDefault();
            const { name, description, visibility } = draft;
            void commands
              .submit(
                id,
                actor,
                { kind: "create", name, description, visibility },
                "Create team",
              )
              .then((value) => {
                if (value?.outcome === "applied") {
                  setCreating(false);
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
              <div className="border-b bg-muted/40 px-5 py-3 font-medium">
                Teams
              </div>
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
                            className="break-words text-base font-semibold"
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
                          <p className="mt-1 whitespace-pre-wrap break-words text-sm text-muted-foreground">
                            {item.team.description}
                          </p>
                        )}
                        <p className="mt-2 text-sm text-muted-foreground">
                          {item.members}{" "}
                          {item.members === 1 ? "member" : "members"}
                          {item.role
                            ? ` · ${item.role === "maintainer" ? "Maintainer" : "Member"}`
                            : ""}
                        </p>
                      </div>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="p-10 text-center text-sm text-muted-foreground">
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
  const state = useTeamRead<Policy>(
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
            <NativeSelect
              id="team-creation"
              value={policy.member_creation ? "members" : "owners"}
              disabled={commands.locked}
              onChange={(event) =>
                void commands.submit(
                  id,
                  actor,
                  {
                    kind: "policy",
                    version: policy.version,
                    member_creation: event.target.value === "members",
                  },
                  "Update team creation permission",
                )
              }
            >
              <NativeSelectOption value="members">
                All organization members
              </NativeSelectOption>
              <NativeSelectOption value="owners">
                Organization owners only
              </NativeSelectOption>
            </NativeSelect>
          </Field>
        </section>
      )}
    </Result>
  );
}
