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

export function RepositoryProperties({ repo }: { repo: Repository }) {
  const request = useRequest<PropertyView>(endpoint(repo, "properties"));
  return (
    <Result state={request} timingLabel="Repository property values">
      {({ schema, values }) =>
        schema.definitions.length ? (
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
                  {definition.description && (
                    <dd className="text-sm text-muted-foreground">
                      {definition.description}
                    </dd>
                  )}
                </div>
              );
            })}
          </dl>
        ) : (
          <p>No repository properties have been defined.</p>
        )
      }
    </Result>
  );
}
