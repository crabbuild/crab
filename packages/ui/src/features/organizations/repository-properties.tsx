import { useState } from "react";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";

export type PropertyValue = boolean | string | string[];
export type PropertyKind =
  | "text"
  | "boolean"
  | "single_select"
  | "multi_select";

export interface PropertyDefinition {
  id: string;
  name: string;
  description: string;
  kind: PropertyKind;
  options: string[];
  default: PropertyValue | null;
}

export interface PropertySchema {
  version: number;
  definitions: PropertyDefinition[];
}

interface PropertyView {
  schema: PropertySchema;
  values: { version: number; overrides: Record<string, PropertyValue> };
}

export function propertySchemaPath(organization: string) {
  return `/api/organizations/${encodeURIComponent(organization)}/repository-properties`;
}

const kinds = {
  text: "Text",
  boolean: "True or false",
  single_select: "Single select",
  multi_select: "Multi select",
} satisfies Record<PropertyKind, string>;

const control = "w-full rounded-md border bg-background px-3 py-2 text-sm";

function display(value: PropertyValue) {
  return Array.isArray(value) ? value.join(", ") : String(value);
}

export function PropertyInput({
  label,
  definition,
  value,
  unset,
  onChange,
}: {
  label: string;
  definition: PropertyDefinition;
  value: PropertyValue | null;
  unset: string;
  onChange: (value: PropertyValue | null) => void;
}) {
  if (definition.kind === "multi_select") {
    const selected = Array.isArray(value) ? value : [];
    return (
      <fieldset className="grid gap-1 rounded-md border p-3">
        <legend className="font-medium">{label}</legend>
        {definition.options.map((option) => (
          <label className="flex items-center gap-2" key={option}>
            <input
              type="checkbox"
              checked={selected.includes(option)}
              onChange={(event) => {
                const next = event.target.checked
                  ? [...selected, option]
                  : selected.filter((item) => item !== option);
                onChange(next.length ? next : null);
              }}
            />
            {option}
          </label>
        ))}
        {!selected.length && (
          <span className="text-sm text-muted-foreground">{unset}</span>
        )}
      </fieldset>
    );
  }
  if (definition.kind === "text") {
    return (
      <label className="grid gap-1">
        {label}
        <Input
          value={typeof value === "string" ? value : ""}
          placeholder={unset}
          maxLength={75}
          onChange={(event) => onChange(event.target.value || null)}
        />
      </label>
    );
  }
  return (
    <label className="grid gap-1">
      {label}
      <select
        className={control}
        value={value === null ? "" : display(value)}
        onChange={(event) => {
          const next = event.target.value;
          onChange(
            next === ""
              ? null
              : definition.kind === "boolean"
                ? next === "true"
                : next,
          );
        }}
      >
        <option value="">{unset}</option>
        {(definition.kind === "boolean"
          ? ["true", "false"]
          : definition.options
        ).map((option) => (
          <option key={option}>{option}</option>
        ))}
      </select>
    </label>
  );
}

export function OrganizationRepositoryProperties({
  organization,
  csrf,
}: {
  organization: string;
  csrf: string;
}) {
  const path = propertySchemaPath(organization);
  const schema = useRequest<PropertySchema>(path);
  const [search, setSearch] = useState("");
  const [selected, setSelected] = useState<Repository>();
  const repositories = useRequest<{
    repositories: Repository[];
    next?: string;
  }>(
    `/api/repos?${new URLSearchParams({ organization, q: search, limit: "50" })}`,
  );
  return (
    <section className="grid gap-6">
      <h2 className="text-lg font-semibold">Repository properties</h2>
      <Result state={schema} timingLabel="Property definitions">
        {(data) => (
          <Definitions
            key={data.version}
            initial={data}
            path={path}
            csrf={csrf}
            onSaved={schema.retry}
          />
        )}
      </Result>
      <div className="grid gap-3">
        <h3 className="font-semibold">Set repository values</h3>
        <label className="grid gap-1">
          Find a repository
          <Input
            type="search"
            maxLength={200}
            value={search}
            onChange={(event) => setSearch(event.target.value)}
          />
        </label>
        <Result state={repositories} timingLabel="Repositories for properties">
          {({ repositories, next }) => (
            <>
              <label className="grid gap-1">
                Repository
                <select
                  id="repository-property-target"
                  className={control}
                  value={selected?.id ?? ""}
                  onChange={(event) => {
                    setSelected(
                      repositories.find(
                        (repo) => repo.id === event.target.value,
                      ),
                    );
                  }}
                >
                  <option value="">Choose a repository</option>
                  {selected &&
                    !repositories.some((repo) => repo.id === selected.id) && (
                      <option value={selected.id}>{selected.name}</option>
                    )}
                  {repositories.map((repo) => (
                    <option key={repo.id} value={repo.id}>
                      {repo.name}
                    </option>
                  ))}
                </select>
              </label>
              {next && (
                <p className="text-sm text-muted-foreground">
                  Refine the search to see more repositories.
                </p>
              )}
            </>
          )}
        </Result>
        {selected && (
          <RepositoryProperties
            key={`${selected.id}:${schema.data?.version ?? ""}`}
            repo={selected}
            csrf={csrf}
          />
        )}
      </div>
    </section>
  );
}

function Definitions({
  initial,
  path,
  csrf,
  onSaved,
}: {
  initial: PropertySchema;
  path: string;
  csrf: string;
  onSaved: () => void;
}) {
  const [schema, setSchema] = useState(initial);
  const [draft, setDraft] = useState<PropertyDefinition>();
  const [options, setOptions] = useState("");
  const [removing, setRemoving] = useState(false);
  const mutation = useMutation(csrf, null);
  const existing = schema.definitions.find((item) => item.id === draft?.id);

  function edit(definition: PropertyDefinition) {
    mutation.reset();
    setOptions(definition.options.join("\n"));
    setDraft(definition);
    setRemoving(false);
  }

  async function save(definitions: PropertyDefinition[]) {
    const result = await mutation.run<PropertySchema>(path, "PUT", {
      expected_version: schema.version,
      definitions,
    });
    if (result) {
      setSchema(result);
      setDraft(undefined);
      onSaved();
    }
  }

  return (
    <div className="grid gap-3">
      <h3 className="font-semibold">Definitions</h3>
      {schema.definitions.map((definition) => (
        <div
          key={definition.id}
          className="flex items-center justify-between gap-4 rounded-md border p-3"
        >
          <div className="min-w-0 break-words">
            <strong>{definition.name}</strong>
            <p className="text-sm text-muted-foreground">
              {kinds[definition.kind]}
              {definition.description && ` · ${definition.description}`}
            </p>
          </div>
          <Button
            type="button"
            variant="outline"
            aria-label={`Edit ${definition.name}`}
            disabled={!!draft}
            onClick={() => edit(definition)}
          >
            Edit
          </Button>
        </div>
      ))}
      {!draft && (
        <Button
          className="justify-self-start"
          type="button"
          onClick={() =>
            edit({
              id: crypto.randomUUID(),
              name: "",
              description: "",
              kind: "text",
              options: [],
              default: null,
            })
          }
        >
          New property
        </Button>
      )}
      {draft && (
        <form
          id="repository-property-definition"
          className="grid gap-3 rounded-md border p-4"
          onSubmit={(event) => {
            event.preventDefault();
            void save(
              existing
                ? schema.definitions.map((item) =>
                    item.id === draft.id ? draft : item,
                  )
                : [...schema.definitions, draft],
            );
          }}
        >
          <label className="grid gap-1">
            Property name
            <Input
              value={draft.name}
              required
              maxLength={75}
              onChange={(event) =>
                setDraft({ ...draft, name: event.target.value })
              }
            />
          </label>
          <label className="grid gap-1">
            Description
            <Input
              value={draft.description}
              maxLength={256}
              onChange={(event) =>
                setDraft({ ...draft, description: event.target.value })
              }
            />
          </label>
          <label className="grid gap-1">
            Property type
            <select
              className={control}
              value={draft.kind}
              disabled={!!existing}
              onChange={(event) => {
                setOptions("");
                setDraft({
                  ...draft,
                  kind: event.target.value as PropertyKind,
                  options: [],
                  default: null,
                });
              }}
            >
              {Object.entries(kinds).map(([kind, name]) => (
                <option key={kind} value={kind}>
                  {name}
                </option>
              ))}
            </select>
          </label>
          {(draft.kind === "single_select" ||
            draft.kind === "multi_select") && (
            <label className="grid gap-1">
              Allowed values, one per line
              <textarea
                className={control}
                required
                value={options}
                onChange={(event) => {
                  setOptions(event.target.value);
                  const next = event.target.value
                    .split("\n")
                    .map((line) => line.trim())
                    .filter(Boolean);
                  const value = draft.default;
                  const retained = Array.isArray(value)
                    ? value.filter((item) => next.includes(item))
                    : value;
                  setDraft({
                    ...draft,
                    options: next,
                    default:
                      (typeof retained === "string" &&
                        !next.includes(retained)) ||
                      (Array.isArray(retained) && !retained.length)
                        ? null
                        : retained,
                  });
                }}
              />
            </label>
          )}
          <PropertyInput
            label="Default value"
            definition={draft}
            value={draft.default}
            unset="No default"
            onChange={(value) => setDraft({ ...draft, default: value })}
          />
          <div className="flex flex-wrap gap-2">
            <Button type="submit" disabled={mutation.pending}>
              Save property
            </Button>
            <Button
              type="button"
              variant="outline"
              onClick={() => setDraft(undefined)}
            >
              Cancel
            </Button>
            {existing && (
              <Button
                type="button"
                variant="outline"
                onClick={() => setRemoving(true)}
              >
                Remove property
              </Button>
            )}
          </div>
          {removing && existing && (
            <div className="flex flex-wrap items-center gap-3">
              <p>Remove {existing.name} and its repository values?</p>
              <Button
                type="button"
                variant="destructive"
                disabled={mutation.pending}
                onClick={() =>
                  void save(
                    schema.definitions.filter(
                      (item) => item.id !== existing.id,
                    ),
                  )
                }
              >
                Confirm removal
              </Button>
            </div>
          )}
        </form>
      )}
      <Failure message={mutation.error} />
      {mutation.conflict && (
        <Button
          className="justify-self-start"
          type="button"
          variant="outline"
          onClick={onSaved}
        >
          Reload definitions
        </Button>
      )}
    </div>
  );
}

export function RepositoryProperties({
  repo,
  csrf,
}: {
  repo: Repository;
  csrf?: string;
}) {
  const path = endpoint(repo, "properties");
  const request = useRequest<PropertyView>(path);
  return (
    <Result state={request} timingLabel="Repository property values">
      {(view) => (
        <Values
          key={`${view.schema.version}:${view.values.version}`}
          initial={view}
          path={path}
          repo={repo}
          csrf={csrf}
          refresh={request.retry}
        />
      )}
    </Result>
  );
}

function Values({
  initial,
  path,
  repo,
  csrf,
  refresh,
}: {
  initial: PropertyView;
  path: string;
  repo: Repository;
  csrf?: string;
  refresh: () => void;
}) {
  const [view, setView] = useState(initial);
  const [overrides, setOverrides] = useState(initial.values.overrides);
  const [saved, setSaved] = useState(false);
  const mutation = useMutation(csrf ?? "", repo);
  if (!view.schema.definitions.length)
    return <p>No repository properties have been defined.</p>;
  return (
    <form
      id="repository-property-values"
      className="grid gap-3 rounded-md border p-4"
      onSubmit={async (event) => {
        event.preventDefault();
        if (!csrf) return;
        const result = await mutation.run<PropertyView>(path, "PUT", {
          schema_version: view.schema.version,
          expected_version: view.values.version,
          overrides,
        });
        if (result) {
          setView(result);
          setOverrides(result.values.overrides);
          setSaved(true);
        }
      }}
    >
      {view.schema.definitions.map((definition) => {
        const value = overrides[definition.id] ?? null;
        const inherited =
          definition.default === null
            ? "Unset"
            : `Default · ${display(definition.default)}`;
        return (
          <div key={definition.id} className="grid gap-1">
            {csrf ? (
              <PropertyInput
                label={definition.name}
                definition={definition}
                value={value}
                unset={inherited}
                onChange={(next) => {
                  setSaved(false);
                  setOverrides((current) => {
                    const updated = { ...current };
                    if (next === null) delete updated[definition.id];
                    else updated[definition.id] = next;
                    return updated;
                  });
                }}
              />
            ) : (
              <p>
                <strong>{definition.name}</strong> ·{" "}
                {value === null ? inherited : display(value)}
              </p>
            )}
            {definition.description && (
              <p className="text-sm text-muted-foreground">
                {definition.description}
              </p>
            )}
          </div>
        );
      })}
      {csrf && (
        <Button
          type="submit"
          className="justify-self-start"
          disabled={mutation.pending}
        >
          Save repository values
        </Button>
      )}
      {saved && <p role="status">Repository values saved.</p>}
      <Failure message={mutation.error} />
      {mutation.conflict && (
        <Button
          className="justify-self-start"
          type="button"
          variant="outline"
          onClick={refresh}
        >
          Reload values
        </Button>
      )}
    </form>
  );
}
