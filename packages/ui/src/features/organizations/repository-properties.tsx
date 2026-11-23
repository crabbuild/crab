import { useState } from "react";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { endpoint, useRequest, type Repository } from "@/lib/api";

import { BulkPropertyValues } from "./bulk-property-values";
import {
  PropertyInput,
  control,
  display,
  propertySchemaPath,
  type PropertyDefinition,
  type PropertyKind,
  type PropertySchema,
  type PropertyValue,
} from "./property-fields";

interface PropertyView {
  schema: PropertySchema;
  values: { version: number; overrides: Record<string, PropertyValue> };
  editable: string[];
}

const kinds = {
  text: "Text",
  boolean: "True or false",
  single_select: "Single select",
  multi_select: "Multi select",
} satisfies Record<PropertyKind, string>;

export function OrganizationRepositoryProperties({
  organization,
  csrf,
}: {
  organization: string;
  csrf: string;
}) {
  const path = propertySchemaPath(organization);
  const schema = useRequest<PropertySchema>(path);
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
      <BulkPropertyValues organization={organization} csrf={csrf} />
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
              {definition.values_editable_by === "repository" &&
                " · Repository administrators can edit values"}
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
              values_editable_by: "organization",
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
          <label className="flex items-start gap-2">
            <input
              type="checkbox"
              className="mt-1"
              checked={draft.values_editable_by === "repository"}
              onChange={(event) =>
                setDraft({
                  ...draft,
                  values_editable_by: event.target.checked
                    ? "repository"
                    : "organization",
                })
              }
            />
            <span>
              Allow repository administrators to set this property
              <span className="block text-sm text-muted-foreground">
                Changing this value can affect which organization rulesets
                apply.
              </span>
            </span>
          </label>
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
  csrf: string;
}) {
  const path = endpoint(repo, "properties");
  const request = useRequest<PropertyView>(path, repo.id);
  return (
    <Result state={request} timingLabel="Repository property values">
      {(view) => (
        <RepositoryValues
          view={view}
          repo={repo}
          path={path}
          csrf={csrf}
          reload={request.retry}
        />
      )}
    </Result>
  );
}

function RepositoryValues({
  view,
  repo,
  path,
  csrf,
  reload,
}: {
  view: PropertyView;
  repo: Repository;
  path: string;
  csrf: string;
  reload: () => void;
}) {
  const [{ schema, values, editable }, setView] = useState(view);
  const [draft, setDraft] = useState<Record<string, PropertyValue> | null>(
    null,
  );
  const [saved, setSaved] = useState(false);
  const mutation = useMutation(csrf, repo);
  const writable = new Set(editable);
  const changed =
    draft &&
    editable.some(
      (id) =>
        JSON.stringify(draft[id]) !== JSON.stringify(values.overrides[id]),
    );

  async function save() {
    if (!draft || !changed) return;
    const result = await mutation.run<PropertyView>(path, "PUT", {
      schema_version: schema.version,
      expected_version: values.version,
      overrides: draft,
    });
    if (result) {
      setView(result);
      setDraft(null);
      setSaved(true);
    }
  }

  if (!schema.definitions.length) {
    return <p>No repository properties have been defined.</p>;
  }

  return (
    <div className="grid gap-3">
      {saved && <p role="status">Repository property values saved.</p>}
      {draft ? (
        <form
          className="grid gap-3"
          onSubmit={(event) => {
            event.preventDefault();
            void save();
          }}
        >
          <fieldset
            className="grid gap-3"
            disabled={mutation.pending || mutation.conflict}
          >
            {schema.definitions
              .filter(({ id }) => writable.has(id))
              .map((definition) => (
                <PropertyInput
                  key={definition.id}
                  label={definition.name}
                  definition={definition}
                  value={draft[definition.id] ?? null}
                  unset={
                    definition.default === null
                      ? "Unset"
                      : `Default · ${display(definition.default)}`
                  }
                  onChange={(value) =>
                    setDraft((current) => {
                      const next = { ...current };
                      if (value === null) delete next[definition.id];
                      else next[definition.id] = value;
                      return next;
                    })
                  }
                />
              ))}
          </fieldset>
          <div className="flex flex-wrap gap-2">
            <Button
              type="submit"
              disabled={!changed || mutation.pending || mutation.conflict}
            >
              Save property values
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={mutation.pending}
              onClick={() => setDraft(null)}
            >
              Cancel
            </Button>
            {mutation.error && (
              <Button type="button" variant="outline" onClick={reload}>
                Reload properties
              </Button>
            )}
          </div>
          <Failure message={mutation.error} />
        </form>
      ) : (
        <>
          <dl className="grid gap-3">
            {schema.definitions.map((definition) => {
              const value = values.overrides[definition.id];
              return (
                <div
                  key={definition.id}
                  className="grid gap-1 rounded-md border p-3"
                >
                  <dt className="font-medium">{definition.name}</dt>
                  <dd>
                    {value !== undefined
                      ? display(value)
                      : definition.default === null
                        ? "Unset"
                        : `Default · ${display(definition.default)}`}
                  </dd>
                  {writable.has(definition.id) &&
                    definition.values_editable_by === "repository" && (
                      <dd className="text-sm text-muted-foreground">
                        Editable by repository administrators
                      </dd>
                    )}
                  {definition.description && (
                    <dd className="text-sm text-muted-foreground">
                      {definition.description}
                    </dd>
                  )}
                </div>
              );
            })}
          </dl>
          {editable.length > 0 && (
            <Button
              type="button"
              variant="outline"
              className="justify-self-start"
              onClick={() => {
                mutation.reset();
                setSaved(false);
                setDraft(values.overrides);
              }}
            >
              Edit properties
            </Button>
          )}
        </>
      )}
    </div>
  );
}
