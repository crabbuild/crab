import { useState } from "react";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { useRequest, type Repository } from "@/lib/api";
import {
  PropertyInput,
  type PropertyDefinition,
} from "@/features/organizations/property-fields";

interface Condition {
  property: string;
  value: string | boolean | null;
  exclude?: boolean;
}

export type RepositoryTarget =
  | { kind: "all" }
  | { kind: "selected"; ids: string[] }
  | { kind: "name"; include: string[]; exclude: string[] }
  | { kind: "properties"; conditions: Condition[] };

export const allRepositories = { kind: "all" } satisfies RepositoryTarget;

export function RepositoryTargetSummary({
  target,
  definitions = [],
}: {
  target: RepositoryTarget;
  definitions?: PropertyDefinition[];
}) {
  switch (target.kind) {
    case "all":
      return <p>All repositories</p>;
    case "selected":
      return (
        <p>
          {target.ids.length} selected{" "}
          {target.ids.length === 1 ? "repository" : "repositories"}
        </p>
      );
    case "name":
      return (
        <p className="break-all">
          Repository names matching <code>{target.include.join(", ")}</code>
          {target.exclude.length > 0 && (
            <>
              {" "}
              · Exclude <code>{target.exclude.join(", ")}</code>
            </>
          )}
        </p>
      );
    case "properties":
      return (
        <ul className="text-sm">
          {target.conditions.map(({ property, value, exclude }, index) => (
            <li className="break-words" key={index}>
              <strong>
                {definitions.find((item) => item.id === property)?.name ??
                  "Repository property"}
              </strong>{" "}
              {exclude ? "excludes" : "matches"} <code>{String(value)}</code>
            </li>
          ))}
        </ul>
      );
  }
}

export function RepositoryTargetEditor({
  organization,
  target,
  onChange,
  definitions = [],
}: {
  organization: string;
  target: RepositoryTarget;
  onChange: (target: RepositoryTarget) => void;
  definitions?: PropertyDefinition[];
}) {
  const [search, setSearch] = useState("");
  const choices = useRequest<{ repositories: Repository[] }>(
    target.kind === "selected"
      ? `/api/repos?${new URLSearchParams({ organization })}`
      : null,
  );
  const [include, setInclude] = useState(
    target.kind === "name" ? target.include.join("\n") : "",
  );
  const [exclude, setExclude] = useState(
    target.kind === "name" ? target.exclude.join("\n") : "",
  );
  const lines = (value: string) =>
    value
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean);
  const styles = "w-full rounded-md border bg-background px-3 py-2 text-sm";

  return (
    <fieldset className="grid gap-2">
      <legend className="font-medium">Target repositories</legend>
      <label className="grid gap-1">
        Repository targeting
        <select
          className={styles}
          value={target.kind}
          onChange={(event) => {
            switch (event.target.value) {
              case "name":
                onChange({
                  kind: "name",
                  include: lines(include),
                  exclude: lines(exclude),
                });
                break;
              case "selected":
                onChange({ kind: "selected", ids: [] });
                break;
              case "properties":
                onChange({ kind: "properties", conditions: [] });
                break;
              default:
                onChange(allRepositories);
            }
          }}
        >
          <option value="all">All repositories</option>
          <option value="selected">Selected repositories</option>
          <option value="name">Matching repository names</option>
          <option value="properties">Matching repository properties</option>
        </select>
      </label>
      {target.kind === "name" && (
        <div className="grid gap-2 sm:grid-cols-2">
          <label className="grid gap-1">
            Include repository patterns
            <textarea
              className={styles}
              placeholder="release-*"
              value={include}
              required
              onChange={(event) => {
                setInclude(event.target.value);
                onChange({ ...target, include: lines(event.target.value) });
              }}
            />
          </label>
          <label className="grid gap-1">
            Exclude repository patterns
            <textarea
              className={styles}
              value={exclude}
              onChange={(event) => {
                setExclude(event.target.value);
                onChange({ ...target, exclude: lines(event.target.value) });
              }}
            />
          </label>
        </div>
      )}
      {target.kind === "selected" && (
        <>
          <label className="grid gap-1">
            Search repositories
            <Input
              type="search"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
            />
          </label>
          <Result state={choices} timingLabel="Organization repository targets">
            {({ repositories }) => {
              const visible = repositories.filter(({ name }) =>
                name.toLowerCase().includes(search.trim().toLowerCase()),
              );
              const unavailable = target.ids.filter(
                (id) => !repositories.some((repo) => repo.id === id),
              );
              return (
                <div className="grid max-h-56 gap-2 overflow-auto rounded-md border p-2">
                  {!visible.length && <p>No repositories match the search.</p>}
                  {visible.map(({ id, name }) => (
                    <label
                      className="flex items-center gap-2 break-all"
                      key={id}
                    >
                      <input
                        type="checkbox"
                        checked={target.ids.includes(id)}
                        onChange={(event) => {
                          onChange({
                            ...target,
                            ids: event.target.checked
                              ? [...target.ids, id]
                              : target.ids.filter((value) => value !== id),
                          });
                        }}
                      />
                      {name}
                    </label>
                  ))}
                  {unavailable.map((id) => (
                    <label className="flex items-center gap-2" key={id}>
                      <input
                        type="checkbox"
                        checked
                        onChange={() =>
                          onChange({
                            ...target,
                            ids: target.ids.filter((value) => value !== id),
                          })
                        }
                      />
                      Unavailable repository{" "}
                      <code className="break-all">{id}</code>
                    </label>
                  ))}
                </div>
              );
            }}
          </Result>
          <RepositoryTargetSummary target={target} />
        </>
      )}
      {target.kind === "properties" && (
        <div className="grid gap-3">
          <p className="text-sm text-muted-foreground">
            Repositories must match every condition.
          </p>
          {!definitions.length && (
            <Link href={`/-/organizations/${organization}?tab=properties`}>
              Manage property definitions
            </Link>
          )}
          {target.conditions.map((condition, index) => {
            const definition = definitions.find(
              (item) => item.id === condition.property,
            );
            const change = (update: Partial<Condition>) =>
              onChange({
                ...target,
                conditions: target.conditions.map((item, position) =>
                  position === index ? { ...item, ...update } : item,
                ),
              });
            return (
              <fieldset
                key={index}
                className="grid gap-2 rounded-md border p-3"
              >
                <legend>Condition {index + 1}</legend>
                <label className="grid gap-1">
                  Property
                  <select
                    className={styles}
                    value={condition.property}
                    onChange={(event) =>
                      change({ property: event.target.value, value: null })
                    }
                  >
                    <option value="">Choose a property</option>
                    {definitions.map(({ id, name }) => (
                      <option key={id} value={id}>
                        {name}
                      </option>
                    ))}
                  </select>
                </label>
                {definition && (
                  <PropertyInput
                    label="Matching value"
                    definition={
                      definition.kind === "multi_select"
                        ? { ...definition, kind: "single_select" }
                        : definition
                    }
                    value={condition.value}
                    unset="Specify a value"
                    onChange={(next) =>
                      change({ value: Array.isArray(next) ? null : next })
                    }
                  />
                )}
                <label className="flex items-center gap-2">
                  <input
                    type="checkbox"
                    checked={!!condition.exclude}
                    onChange={(event) =>
                      change({ exclude: event.target.checked || undefined })
                    }
                  />
                  Exclude repositories with this value
                </label>
                <Button
                  className="justify-self-start"
                  type="button"
                  variant="outline"
                  onClick={() =>
                    onChange({
                      ...target,
                      conditions: target.conditions.filter(
                        (_, position) => position !== index,
                      ),
                    })
                  }
                >
                  Remove condition
                </Button>
              </fieldset>
            );
          })}
          <Button
            className="justify-self-start"
            type="button"
            variant="outline"
            disabled={!definitions.length || target.conditions.length >= 20}
            onClick={() =>
              onChange({
                ...target,
                conditions: [
                  ...target.conditions,
                  { property: "", value: null },
                ],
              })
            }
          >
            Add condition
          </Button>
        </div>
      )}
    </fieldset>
  );
}
