import { useState } from "react";
import { Input } from "@/components/ui/input";
import { Result } from "@/components/request/result";
import { useRequest, type Repository } from "@/lib/api";

export type RepositoryTarget =
  | { kind: "all" }
  | { kind: "selected"; ids: string[] }
  | { kind: "name"; include: string[]; exclude: string[] };

export const allRepositories = { kind: "all" } satisfies RepositoryTarget;

export function RepositoryTargetSummary({
  target,
}: {
  target: RepositoryTarget;
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
  }
}

export function RepositoryTargetEditor({
  organization,
  target,
  onChange,
}: {
  organization: string;
  target: RepositoryTarget;
  onChange: (target: RepositoryTarget) => void;
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
              default:
                onChange(allRepositories);
            }
          }}
        >
          <option value="all">All repositories</option>
          <option value="selected">Selected repositories</option>
          <option value="name">Matching repository names</option>
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
    </fieldset>
  );
}
