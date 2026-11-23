import { useState } from "react";
import { UsersIcon } from "lucide-react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import type {
  TeamAccessState,
  TeamAccessUpdate,
  TeamGrant,
} from "@/lib/contracts";
import { teamHref } from "../organizations/teams/model";
import { TeamPicker } from "./team-picker";

const roles = [
  { value: "read", label: "Read" },
  { value: "triage", label: "Triage" },
  { value: "write", label: "Write" },
  { value: "admin", label: "Admin" },
] as const;

export function RepositoryTeamSettings({
  repo,
  organization,
  csrf,
}: {
  repo: Repository;
  organization: string;
  csrf: string;
}) {
  const url = endpoint(repo, "team-access");
  const state = useRequest<TeamAccessState>(url);
  return (
    <section className="settings-content" aria-labelledby="team-access-title">
      <div className="settings-title">
        <div>
          <h2 id="team-access-title">Team access</h2>
          <p>
            Give teams in the owning organization access to this repository.
          </p>
        </div>
      </div>
      <Result state={state} timingLabel="Repository team access">
        {(data) =>
          data.organization.id === organization ? (
            <GrantEditor
              initial={data}
              url={url}
              repositoryId={repo.id}
              csrf={csrf}
              reload={state.retry}
            />
          ) : (
            <p role="alert">Repository ownership changed. Reload the page.</p>
          )
        }
      </Result>
    </section>
  );
}

function GrantEditor({
  initial,
  url,
  repositoryId,
  csrf,
  reload,
}: {
  initial: TeamAccessState;
  url: string;
  repositoryId: string;
  csrf: string;
  reload: () => void;
}) {
  const [state, setState] = useState(initial);
  const [draft, setDraft] = useState(state.organization.teams);
  const [names, setNames] = useState(state.team_names);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string>();
  const [reloadNeeded, setReloadNeeded] = useState(false);
  const original = state.organization.teams;
  const selected = new Set(draft.map((grant) => grant.team));
  const changed =
    draft.length !== original.length ||
    original.some(
      (grant) =>
        !draft.some(
          (item) => item.team === grant.team && item.access === grant.access,
        ),
    );
  const unavailable = draft.some((grant) => !names[grant.team]);
  const locked = saving || reloadNeeded;
  const organization = state.organization.id;

  function edit(next: TeamGrant[]) {
    setDraft(next);
    setSaved(false);
    setError(undefined);
  }

  async function save() {
    if (locked || !changed || unavailable) return;
    setSaving(true);
    setSaved(false);
    setError(undefined);
    try {
      const update: TeamAccessUpdate = {
        expected_revision: state.revision,
        teams: draft,
      };
      const response = await fetch(url, {
        method: "PUT",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
          "X-CSRF-Token": csrf,
          "X-Crab-Repository-Id": repositoryId,
        },
        body: JSON.stringify(update),
      });
      if (response.status === 401)
        window.dispatchEvent(new Event("crab-session-expired"));
      if (
        response.status === 409 ||
        response.status === 404 ||
        response.status === 403
      ) {
        setReloadNeeded(true);
        setError(
          response.status === 409
            ? "Repository access changed while you were editing. Reload to review the latest grants."
            : "Your access to these settings changed. Reload to check your permissions.",
        );
        return;
      }
      const body = await response.json();
      if (!response.ok) {
        setError(body?.error?.message ?? "Team access could not be saved.");
        return;
      }
      const current: TeamAccessState = body;
      setState(current);
      setDraft(current.organization.teams);
      setNames(current.team_names);
      setSaved(true);
    } catch {
      setError(
        "Couldn’t confirm that team access was saved. Try again or reload the grants.",
      );
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="max-w-prose text-muted-foreground">
          Members get their highest role across team, individual, and SSO group
          grants.
        </p>
        <TeamPicker
          organization={organization}
          selected={selected}
          disabled={locked || draft.length >= 100}
          onSelect={(team) => {
            setNames({ ...names, [team.id]: team.name });
            edit([...draft, { team: team.id, access: "read" }]);
          }}
        />
      </div>
      {draft.length ? (
        <ul
          aria-label="Repository team grants"
          className="m-0 list-none divide-y rounded-lg border p-0"
        >
          {draft.map((grant) => {
            const name = names[grant.team];
            const label = name ?? `Unavailable team ${grant.team}`;
            return (
              <li
                key={grant.team}
                className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-2 p-3 sm:grid-cols-[minmax(0,1fr)_auto_auto] sm:gap-4"
              >
                <div className="col-span-2 flex min-w-0 items-center gap-2 wrap-anywhere sm:col-span-1">
                  <UsersIcon
                    aria-hidden="true"
                    className="size-4 shrink-0 text-muted-foreground"
                  />
                  {name ? (
                    <Link
                      href={teamHref(organization, grant.team)}
                      className="min-w-0 font-medium"
                    >
                      {name}
                    </Link>
                  ) : (
                    <span className="min-w-0 text-muted-foreground">
                      {label}
                    </span>
                  )}
                </div>
                <NativeSelect
                  aria-label={`Role for ${label}`}
                  className="w-28"
                  disabled={locked || !name}
                  value={grant.access}
                  onChange={(event) => {
                    const role = roles.find(
                      (item) => item.value === event.target.value,
                    );
                    if (role)
                      edit(
                        draft.map((item) =>
                          item.team === grant.team
                            ? { ...item, access: role.value }
                            : item,
                        ),
                      );
                  }}
                >
                  {roles.map(({ value, label }) => (
                    <NativeSelectOption key={value} value={value}>
                      {label}
                    </NativeSelectOption>
                  ))}
                </NativeSelect>
                <Button
                  variant="outline"
                  className="justify-self-end"
                  aria-label={`Remove ${label}`}
                  disabled={locked}
                  onClick={() =>
                    edit(draft.filter((item) => item.team !== grant.team))
                  }
                >
                  Remove
                </Button>
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="rounded-lg border p-4 text-muted-foreground">
          No teams have access through a team grant.
        </p>
      )}
      {unavailable && (
        <p role="status">Remove grants for unavailable teams before saving.</p>
      )}
      <details className="text-muted-foreground">
        <summary className="w-fit cursor-pointer">
          What can each role do?
        </summary>
        <dl className="mt-2 grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-1">
          <dt className="font-medium">Read</dt>
          <dd>View and clone the repository.</dd>
          <dt className="font-medium">Triage</dt>
          <dd>Manage issues and pull requests without pushing code.</dd>
          <dt className="font-medium">Write</dt>
          <dd>Push changes and collaborate on the repository.</dd>
          <dt className="font-medium">Admin</dt>
          <dd>Manage repository settings and direct collaborator access.</dd>
        </dl>
      </details>
      {error && (
        <div className="notice error" role="alert">
          <p>{error}</p>
          <Button variant="outline" onClick={reload}>
            Reload grants
          </Button>
        </div>
      )}
      {saved && <p role="status">Team access saved.</p>}
      <div className="flex flex-wrap items-center gap-2">
        <Button
          disabled={locked || !changed || unavailable}
          onClick={() => void save()}
        >
          {saving ? "Saving…" : "Save team access"}
        </Button>
        {changed && !reloadNeeded && (
          <Button
            variant="outline"
            disabled={saving}
            onClick={() => edit(original)}
          >
            Discard changes
          </Button>
        )}
      </div>
    </div>
  );
}
