import { useState, type FormEvent } from "react";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import type { RepositoryMember, TeamListing } from "@/lib/contracts";

type Role = RepositoryMember["access"];

const roles: Record<Role, string> = {
  read: "Read",
  triage: "Triage",
  write: "Write",
  admin: "Admin",
};

interface TeamGrant {
  team: string;
  name: string | null;
  access: Role;
}

interface TeamAccessState {
  revision: number;
  grants: TeamGrant[];
}

type Edit = {
  team: string;
  access: Role;
  name: string | null;
  revision: number;
  remove: boolean;
};

export function TeamAccessSettings({
  repo,
  csrf,
  onChanged,
}: {
  repo: Repository & { organization: string };
  csrf: string;
  onChanged: (expectedRevision: number, revision: number) => void;
}) {
  const url = endpoint(repo, "settings/team-access");
  const request = useRequest<TeamAccessState>(url);
  return (
    <section className="settings-section" aria-labelledby="team-access-heading">
      <header>
        <div>
          <h3 id="team-access-heading">Team access</h3>
          <p>Grant repository access to teams in this organization.</p>
        </div>
      </header>
      <Result state={request} timingLabel="Repository team access">
        {(state) => (
          <TeamAccessForm
            repo={repo}
            csrf={csrf}
            state={state}
            onChanged={onChanged}
            reload={request.retry}
          />
        )}
      </Result>
    </section>
  );
}

function TeamAccessForm({
  repo,
  csrf,
  state,
  onChanged,
  reload,
}: {
  repo: Repository & { organization: string };
  csrf: string;
  state: TeamAccessState;
  onChanged: (expectedRevision: number, revision: number) => void;
  reload: () => void;
}) {
  const mutation = useMutation(csrf, repo);
  const [draft, setDraft] = useState<Edit>();
  const [adding, setAdding] = useState(false);

  function open(grant: TeamGrant, remove: boolean) {
    mutation.reset();
    setAdding(false);
    setDraft({ ...grant, revision: state.revision, remove });
  }

  function close() {
    mutation.reset();
    setAdding(false);
    setDraft(undefined);
  }

  async function save(event: FormEvent) {
    event.preventDefault();
    if (!draft || draft.revision !== state.revision) return;
    const body = draft.remove
      ? { expected_revision: draft.revision }
      : { expected_revision: draft.revision, access: draft.access };
    const completed = await mutation.run<{ revision: number }>(
      endpoint(repo, `settings/team-access/${encodeURIComponent(draft.team)}`),
      draft.remove ? "DELETE" : "PUT",
      body,
    );
    if (completed) {
      onChanged(draft.revision, completed.revision);
      close();
      reload();
    }
  }

  const stale = draft !== undefined && draft.revision !== state.revision;
  return (
    <div className="grid gap-3 p-4">
      {state.grants.length ? (
        <ul className="grid divide-y rounded-md border">
          {state.grants.map((grant) => (
            <li
              key={grant.team}
              className="flex flex-wrap items-center justify-between gap-2 p-3"
            >
              <div className="min-w-0">
                <strong className="break-words">
                  {grant.name ?? "Deleted team"}
                </strong>
                <p className="text-sm text-muted-foreground">
                  {roles[grant.access]}
                </p>
              </div>
              <div className="flex flex-wrap gap-2">
                {grant.name && (
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    disabled={mutation.pending}
                    onClick={() => open(grant, false)}
                    aria-label={`Change access for ${grant.name}`}
                  >
                    Change role
                  </Button>
                )}
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  disabled={mutation.pending}
                  onClick={() => open(grant, true)}
                  aria-label={`Remove access for ${grant.name ?? "deleted team"}`}
                >
                  Remove
                </Button>
              </div>
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-sm text-muted-foreground">
          No team grants are visible.
        </p>
      )}
      {!draft && !adding && (
        <Button
          type="button"
          variant="outline"
          className="justify-self-start"
          onClick={() => {
            setAdding(true);
          }}
        >
          Add team
        </Button>
      )}
      {adding && (
        <TeamPicker
          organization={repo.organization}
          assigned={state.grants}
          onCancel={close}
          onChoose={(team) => open(team, false)}
        />
      )}
      {draft && (
        <form
          className="grid gap-3 rounded-md border p-3"
          onSubmit={(event) => void save(event)}
        >
          <h4 className="font-semibold">
            {draft.remove ? "Remove access" : "Set access"} for{" "}
            {draft.name ?? "deleted team"}
          </h4>
          {!draft.remove && (
            <label className="flex flex-col gap-1">
              Repository role
              <select
                className="w-fit rounded-md border px-2 py-1"
                value={draft.access}
                disabled={mutation.pending}
                onChange={(event) =>
                  setDraft({ ...draft, access: event.target.value as Role })
                }
              >
                {Object.entries(roles).map(([value, label]) => (
                  <option key={value} value={value}>
                    {label}
                  </option>
                ))}
              </select>
            </label>
          )}
          {stale && (
            <p role="alert">
              Repository access changed. Reopen this edit to use the latest
              access list.
            </p>
          )}
          <Failure message={mutation.error} />
          <div className="flex flex-wrap gap-2">
            <Button
              type="submit"
              disabled={mutation.pending || stale}
              variant={draft.remove ? "destructive" : "default"}
            >
              {mutation.pending
                ? "Saving…"
                : draft.remove
                  ? "Remove team access"
                  : "Save team role"}
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={mutation.pending}
              onClick={close}
            >
              Cancel
            </Button>
            {mutation.error && (
              <Button type="button" variant="outline" onClick={reload}>
                Reload team access
              </Button>
            )}
          </div>
        </form>
      )}
    </div>
  );
}

function TeamPicker({
  organization,
  assigned,
  onChoose,
  onCancel,
}: {
  organization: string;
  assigned: TeamGrant[];
  onChoose: (team: TeamGrant) => void;
  onCancel: () => void;
}) {
  const [query, setQuery] = useState("");
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState<string>();
  const params = new URLSearchParams({ search });
  if (after) params.set("after", after);
  const teams = useRequest<TeamListing>(
    `/api/organizations/${encodeURIComponent(organization)}/teams?${params}`,
  );
  return (
    <div className="grid gap-3 rounded-md border p-3">
      <form
        className="flex flex-wrap items-end gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          setAfter(undefined);
          setSearch(query.trim());
        }}
      >
        <label className="grid min-w-0 flex-1 gap-1">
          Find a team
          <Input
            type="search"
            value={query}
            maxLength={100}
            onChange={(event) => setQuery(event.target.value)}
          />
        </label>
        <Button type="submit" variant="outline">
          Search teams
        </Button>
        <Button type="button" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </form>
      <Result state={teams} timingLabel="Available organization teams">
        {({ items, next }) => (
          <div className="grid gap-2">
            {items.length ? (
              <ul className="grid divide-y">
                {items.map(({ team }) => (
                  <li
                    key={team.id}
                    className="flex flex-wrap items-center justify-between gap-2 py-2"
                  >
                    <span className="min-w-0 break-words">{team.name}</span>
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      aria-label={`Choose ${team.name}`}
                      disabled={assigned.some(
                        (grant) => grant.team === team.id,
                      )}
                      onClick={() =>
                        onChoose({
                          team: team.id,
                          name: team.name,
                          access: "read",
                        })
                      }
                    >
                      Choose
                    </Button>
                  </li>
                ))}
              </ul>
            ) : (
              <p>No teams match this search.</p>
            )}
            {next && (
              <Button
                type="button"
                variant="outline"
                className="justify-self-start"
                onClick={() => setAfter(next)}
              >
                Next teams
              </Button>
            )}
          </div>
        )}
      </Result>
    </div>
  );
}
