import { Checkbox } from "@/components/ui/checkbox";
import { Textarea } from "@/components/ui/textarea";
import { FieldSelect } from "@/components/ui/field-select";
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

type Condition = ({ property: string } | { system: "fork" | "name" }) & {
  value: string | boolean | null;
  exclude?: boolean;
};

const systemNames = { fork: "Repository type", name: "Repository name" };

export type RepositoryTarget =
  | { kind: "all" }
  | { kind: "selected"; ids: string[] }
  | { kind: "name"; include: string[]; exclude: string[]; protected?: boolean }
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
          {target.protected && <> · Renaming restricted</>}
        </p>
      );
    case "properties":
      return (
        <ul className="text-sm">
          {target.conditions.map((condition, index) => (
            <li className="break-words" key={index}>
              <strong>
                {"system" in condition
                  ? systemNames[condition.system]
                  : (definitions.find((item) => item.id === condition.property)
                      ?.name ?? "Repository property")}
              </strong>{" "}
              {condition.exclude ? "excludes" : "matches"}{" "}
              <code>
                {"system" in condition && condition.system === "fork"
                  ? condition.value === true
                    ? "Fork"
                    : "Source repository"
                  : String(condition.value)}
              </code>
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
        <FieldSelect
          className="w-full"
          value={target.kind}
          onValueChange={(nextValue) => {
            switch (nextValue) {
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
          options={[
            { value: "all", label: "All repositories" },
            { value: "selected", label: "Selected repositories" },
            { value: "name", label: "Matching repository names" },
            { value: "properties", label: "Matching repository properties" },
          ]}
        />
      </label>
      {target.kind === "name" && (
        <div className="grid gap-2 sm:grid-cols-2">
          <label className="grid gap-1">
            Include repository patterns
            <Textarea
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
            <Textarea
              className={styles}
              value={exclude}
              onChange={(event) => {
                setExclude(event.target.value);
                onChange({ ...target, exclude: lines(event.target.value) });
              }}
            />
          </label>
          <label className="flex items-center gap-2 sm:col-span-2">
            <Checkbox
              checked={!!target.protected}
              onCheckedChange={(checked) =>
                onChange({
                  ...target,
                  protected: checked === true || undefined,
                })
              }
            />
            Prevent renaming of target repositories
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
                      <Checkbox
                        checked={target.ids.includes(id)}
                        onCheckedChange={(checked) => {
                          onChange({
                            ...target,
                            ids:
                              checked === true
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
                      <Checkbox
                        checked
                        onCheckedChange={() =>
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
            const definition =
              "property" in condition
                ? definitions.find((item) => item.id === condition.property)
                : undefined;
            const change = (replacement: Condition) =>
              onChange({
                ...target,
                conditions: target.conditions.map((item, position) =>
                  position === index ? replacement : item,
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
                  <FieldSelect
                    className="w-full"
                    value={
                      "system" in condition
                        ? `system:${condition.system}`
                        : condition.property
                    }
                    onValueChange={(key) => {
                      const next = { value: null, exclude: condition.exclude };
                      if (key === "system:fork")
                        change({ ...next, system: "fork" });
                      else if (key === "system:name")
                        change({ ...next, system: "name" });
                      else change({ ...next, property: key });
                    }}
                    options={[
                      { value: "", label: "Choose a property" },
                      {
                        value: "system:fork",
                        label: "Repository type",
                        group: "Built-in properties",
                      },
                      {
                        value: "system:name",
                        label: "Repository name",
                        group: "Built-in properties",
                      },
                      ...definitions.map(({ id, name }) => ({
                        value: id,
                        label: name,
                        group: "Custom properties",
                      })),
                    ]}
                  />
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
                      change({
                        ...condition,
                        value: Array.isArray(next) ? null : next,
                      })
                    }
                  />
                )}
                {"system" in condition &&
                  (condition.system === "fork" ? (
                    <label className="grid gap-1">
                      Matching value
                      <FieldSelect
                        className="w-full"
                        value={
                          condition.value === null
                            ? ""
                            : String(condition.value)
                        }
                        onValueChange={(nextValue) =>
                          change({
                            ...condition,
                            value:
                              nextValue === "" ? null : nextValue === "true",
                          })
                        }
                        options={[
                          { value: "", label: "Choose a repository type" },
                          { value: "false", label: "Source repository" },
                          { value: "true", label: "Fork" },
                        ]}
                      />
                    </label>
                  ) : (
                    <label className="grid gap-1">
                      Matching value
                      <Input
                        placeholder="release-*"
                        value={
                          typeof condition.value === "string"
                            ? condition.value
                            : ""
                        }
                        onChange={(event) =>
                          change({
                            ...condition,
                            value: event.target.value || null,
                          })
                        }
                      />
                    </label>
                  ))}
                <label className="flex items-center gap-2">
                  <Checkbox
                    checked={!!condition.exclude}
                    onCheckedChange={(checked) =>
                      change({
                        ...condition,
                        exclude: checked === true || undefined,
                      })
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
            disabled={target.conditions.length >= 20}
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
