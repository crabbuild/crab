import { useId, useState } from "react";
import { PlusIcon, TrashIcon } from "lucide-react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Field,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import type { TeamListing } from "@/lib/contracts";

type Alias = { id: string; slug: string };
type Settings = {
  organization: string | null;
  namespace: string;
  teams: Alias[];
  version: number;
};

const uuid = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i;
const slug = /^(?=.{1,64}$)[a-z\d]+(?:-[a-z\d]+)*$/;

export function MentionSettings({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf: string;
}) {
  const id = useId();
  const url = endpoint(repo, "mention-settings");
  const resource = useRequest<Settings>(url);
  const mutation = useMutation(csrf, repo);
  const latest = useRequest<Settings>(mutation.conflict ? url : null);
  const [saved, setSaved] = useState<Settings>();
  const [draft, setDraft] = useState<Settings>();
  const [feedback, setFeedback] = useState("");
  const baseline = saved ?? resource.data;
  const current = draft ?? baseline;

  function change(next: Settings) {
    setDraft(next);
    setFeedback("");
    mutation.reset();
  }

  return (
    <section className="settings-content" aria-labelledby={`${id}-heading`}>
      <div className="settings-title">
        <div>
          <h2 id={`${id}-heading`}>Team mentions</h2>
          <p>
            Make selected organization teams available for mentions in this
            repository.
          </p>
        </div>
      </div>
      <Result state={resource} timingLabel="Team mention settings">
        {() => {
          if (!current) return null;
          const organization = current.organization?.trim();
          const validOrganization =
            organization &&
            uuid.test(organization) &&
            organization !== "00000000-0000-0000-0000-000000000000"
              ? organization
              : undefined;
          const disabled =
            repo.archived || mutation.pending || mutation.conflict;
          const valid =
            current.organization === null ||
            (validOrganization &&
              slug.test(current.namespace) &&
              current.teams.every((alias) => slug.test(alias.slug)) &&
              new Set(current.teams.map((alias) => alias.slug)).size ===
                current.teams.length);
          const changed = JSON.stringify(current) !== JSON.stringify(baseline);

          return (
            <form
              className="settings-section"
              onSubmit={async (event) => {
                event.preventDefault();
                if (disabled || !valid || !changed) return;
                const result = await mutation.run<Settings>(url, "PUT", {
                  ...current,
                  organization: validOrganization ?? null,
                });
                if (result) {
                  setSaved(result);
                  setDraft(undefined);
                  setFeedback("Team mention settings saved.");
                }
              }}
            >
              <FieldSet disabled={disabled} className="min-w-0 gap-5">
                <FieldGroup className="grid gap-4 sm:grid-cols-2">
                  <Field className="min-w-0">
                    <FieldLabel htmlFor={`${id}-organization`}>
                      Organization ID
                    </FieldLabel>
                    <Input
                      id={`${id}-organization`}
                      value={current.organization ?? ""}
                      maxLength={36}
                      autoComplete="off"
                      spellCheck={false}
                      aria-describedby={`${id}-organization-help`}
                      onChange={(event) => {
                        const value = event.target.value;
                        change({
                          ...current,
                          organization: value || null,
                          namespace: value ? current.namespace : "",
                          teams:
                            value === baseline?.organization
                              ? baseline.teams
                              : [],
                        });
                      }}
                    />
                    <FieldDescription id={`${id}-organization-help`}>
                      Copy the ID after <code>/-/organizations/</code> in an
                      organization you own.
                    </FieldDescription>
                  </Field>
                  <Field className="min-w-0">
                    <FieldLabel htmlFor={`${id}-namespace`}>
                      Mention namespace
                    </FieldLabel>
                    <Input
                      id={`${id}-namespace`}
                      value={current.namespace}
                      maxLength={64}
                      disabled={current.organization === null}
                      autoComplete="off"
                      spellCheck={false}
                      placeholder="engineering"
                      aria-describedby={`${id}-namespace-help`}
                      onChange={(event) =>
                        change({ ...current, namespace: event.target.value })
                      }
                    />
                    <FieldDescription id={`${id}-namespace-help`}>
                      Use lowercase letters, numbers, or single hyphens. This is
                      the `org` in <code>@org/team</code>.
                    </FieldDescription>
                  </Field>
                </FieldGroup>
                {validOrganization && (
                  <TeamAliases
                    key={validOrganization}
                    organization={validOrganization}
                    namespace={current.namespace}
                    aliases={current.teams}
                    disabled={disabled}
                    onChange={(teams) => change({ ...current, teams })}
                  />
                )}
                <div className="flex flex-wrap gap-2">
                  <Button
                    type="submit"
                    disabled={disabled || !valid || !changed}
                  >
                    {mutation.pending ? "Saving…" : "Save settings"}
                  </Button>
                  {current.organization !== null && (
                    <Button
                      variant="outline"
                      onClick={() =>
                        change({
                          ...current,
                          organization: null,
                          namespace: "",
                          teams: [],
                        })
                      }
                    >
                      Clear binding
                    </Button>
                  )}
                </div>
              </FieldSet>
              {feedback && (
                <p className="settings-help" role="status">
                  {feedback}
                </p>
              )}
              {mutation.error && (
                <p className="settings-help error" role="alert">
                  {mutation.error}
                </p>
              )}
              {mutation.conflict && (
                <div className="flex flex-wrap items-center gap-2 pt-3">
                  <Button
                    variant="outline"
                    disabled={!latest.data}
                    onClick={() => {
                      if (!latest.data) return;
                      setSaved(latest.data);
                      setDraft(undefined);
                      mutation.reset();
                    }}
                  >
                    Discard draft and load saved settings
                  </Button>
                  {latest.loading && (
                    <span role="status">Loading saved settings…</span>
                  )}
                  {latest.error && (
                    <Button variant="ghost" onClick={latest.retry}>
                      Retry loading saved settings
                    </Button>
                  )}
                </div>
              )}
            </form>
          );
        }}
      </Result>
    </section>
  );
}

function TeamAliases({
  organization,
  namespace,
  aliases,
  disabled,
  onChange,
}: {
  organization: string;
  namespace: string;
  aliases: Alias[];
  disabled: boolean;
  onChange: (aliases: Alias[]) => void;
}) {
  const id = useId();
  const [query, setQuery] = useState("");
  const [search, setSearch] = useState("");
  const [after, setAfter] = useState<string>();
  const [selected, setSelected] = useState("");
  const [addedNames, setAddedNames] = useState<Record<string, string>>({});
  const params = new URLSearchParams({ search, limit: "50" });
  if (after) params.set("after", after);
  const listing = useRequest<TeamListing>(
    `/api/organizations/${encodeURIComponent(organization)}/teams?${params}`,
  );
  const closed =
    listing.data?.items.filter(({ team }) => team.visibility === "closed") ??
    [];
  const available = closed.filter(
    ({ team }) => !aliases.some((alias) => alias.id === team.id),
  );
  const selection = available.find(({ team }) => team.id === selected)?.team;

  function findTeams() {
    setSearch(query.trim());
    setAfter(undefined);
    setSelected("");
  }

  return (
    <FieldSet disabled={disabled} className="min-w-0 gap-4">
      <FieldLegend>Team aliases</FieldLegend>
      <FieldDescription>
        Only closed teams can be added. Team aliases do not grant repository
        access.
      </FieldDescription>
      {aliases.map((alias) => {
        const name =
          closed.find(({ team }) => team.id === alias.id)?.team.name ??
          addedNames[alias.id];
        return (
          <div
            key={alias.id}
            className="grid grid-cols-[minmax(0,1fr)_auto] items-end gap-2 sm:max-w-md"
          >
            <Field className="min-w-0">
              <FieldLabel htmlFor={`${id}-${alias.id}`} className="break-all">
                Alias for {name ?? alias.id}
              </FieldLabel>
              <Input
                id={`${id}-${alias.id}`}
                value={alias.slug}
                maxLength={64}
                autoComplete="off"
                spellCheck={false}
                placeholder="docs-team"
                onChange={(event) =>
                  onChange(
                    aliases.map((team) =>
                      team.id === alias.id
                        ? { ...team, slug: event.target.value }
                        : team,
                    ),
                  )
                }
              />
              {namespace && alias.slug && (
                <FieldDescription className="break-all">
                  <code>
                    @{namespace}/{alias.slug}
                  </code>
                </FieldDescription>
              )}
            </Field>
            <Button
              variant="ghost"
              size="icon"
              aria-label={`Remove alias for ${name ?? alias.id}`}
              onClick={() =>
                onChange(aliases.filter((team) => team.id !== alias.id))
              }
            >
              <TrashIcon />
            </Button>
          </div>
        );
      })}
      <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] sm:items-end">
        <Field>
          <FieldLabel htmlFor={`${id}-search`}>Find a team</FieldLabel>
          <div className="flex gap-2">
            <Input
              id={`${id}-search`}
              value={query}
              maxLength={100}
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== "Enter") return;
                event.preventDefault();
                findTeams();
              }}
            />
            <Button variant="outline" onClick={findTeams}>
              Search
            </Button>
          </div>
        </Field>
        <Field>
          <FieldLabel htmlFor={`${id}-team`}>Closed team</FieldLabel>
          <NativeSelect
            id={`${id}-team`}
            className="w-full"
            value={selection ? selected : ""}
            disabled={available.length === 0}
            onChange={(event) => setSelected(event.target.value)}
          >
            <NativeSelectOption value="">Choose a team</NativeSelectOption>
            {available.map(({ team }) => (
              <NativeSelectOption key={team.id} value={team.id}>
                {team.name}
              </NativeSelectOption>
            ))}
          </NativeSelect>
        </Field>
        <Button
          variant="outline"
          disabled={disabled || !selection || aliases.length >= 24}
          onClick={() => {
            if (!selection) return;
            setAddedNames((names) => ({
              ...names,
              [selection.id]: selection.name,
            }));
            onChange([...aliases, { id: selection.id, slug: "" }]);
            setSelected("");
          }}
        >
          <PlusIcon data-icon="inline-start" /> Add team
        </Button>
      </div>
      {listing.loading && (
        <FieldDescription role="status">Loading teams…</FieldDescription>
      )}
      {listing.error && (
        <div className="flex flex-wrap items-center gap-2">
          <p className="error" role="alert">
            Could not load teams. {listing.error}
          </p>
          <Button
            variant="outline"
            onClick={() => {
              setAfter(undefined);
              listing.retry();
            }}
          >
            Retry
          </Button>
        </div>
      )}
      {listing.data && available.length === 0 && (
        <FieldDescription role="status">
          No closed teams are available on this page.
        </FieldDescription>
      )}
      {listing.data?.next && (
        <Button
          variant="outline"
          className="self-start"
          onClick={() => {
            setAfter(listing.data?.next ?? undefined);
            setSelected("");
          }}
        >
          More teams
        </Button>
      )}
    </FieldSet>
  );
}
