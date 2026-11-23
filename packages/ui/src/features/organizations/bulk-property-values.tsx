import { useState } from "react";
import { Failure } from "@/components/discussion/failure";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { useRequest, type Repository } from "@/lib/api";
import {
  control,
  display,
  PropertyInput,
  propertySchemaPath,
  type PropertySchema,
  type PropertyValue,
} from "./property-fields";

const selectionLimit = 50;

type CurrentValue =
  | { kind: "shared"; value: PropertyValue | null }
  | { kind: "mixed" };
type Edit = { kind: "set"; value: PropertyValue | null } | { kind: "default" };
interface SelectionView {
  schema: PropertySchema;
  repositories: {
    repository: { id: string; ownership_epoch: number };
    version: number;
  }[];
  effective: Record<string, CurrentValue>;
}
interface Submission {
  request_id: string;
  issued_at_ms: number;
  schema_version: number;
  repositories: SelectionView["repositories"];
  changes: Record<string, PropertyValue | null>;
}

export function BulkPropertyValues({
  organization,
  csrf,
}: {
  organization: string;
  csrf: string;
}) {
  const [search, setSearch] = useState("");
  const [pages, setPages] = useState<string[]>([]);
  const [selected, setSelected] = useState<Record<string, string>>({});
  const [editing, setEditing] = useState(false);
  const [notice, setNotice] = useState("");
  const ids = Object.keys(selected);
  const after = pages.at(-1);
  const params = new URLSearchParams({
    organization,
    q: search,
    limit: String(selectionLimit),
  });
  if (after) params.set("after", after);
  const result = useRequest<{
    repositories: Repository[];
    next: string | null;
  }>(editing ? null : `/api/repos?${params}`);
  const path = `${propertySchemaPath(organization)}/values`;
  const selection = useRequest<SelectionView>(
    editing
      ? `${path}?${new URLSearchParams({ repositories: ids.join(",") })}`
      : null,
  );

  function toggle(repo: Repository, checked: boolean) {
    setNotice("");
    setSelected((current) => {
      const next = { ...current };
      if (checked) next[repo.id] = repo.name;
      else delete next[repo.id];
      return next;
    });
  }

  return (
    <div
      className="grid gap-3"
      aria-labelledby="repository-property-values-heading"
    >
      <h3 id="repository-property-values-heading" className="font-semibold">
        Set repository values
      </h3>
      {notice && <p role="status">{notice}</p>}
      {editing ? (
        <>
          <div className="flex flex-wrap items-start justify-between gap-2">
            <div>
              <p className="font-medium">
                Editing {ids.length}{" "}
                {ids.length === 1 ? "repository" : "repositories"}
              </p>
              <p className="text-sm text-muted-foreground break-words">
                {Object.values(selected).join(", ")}
              </p>
            </div>
          </div>
          <Result
            state={selection}
            timingLabel="Selected repository properties"
          >
            {(view) => (
              <SelectionEditor
                view={view}
                path={path}
                csrf={csrf}
                reload={selection.retry}
                cancel={() => setEditing(false)}
                saved={(updated) => {
                  setEditing(false);
                  setNotice(
                    updated === 0
                      ? "Selected values are up to date."
                      : `Repository values saved for ${updated} ${updated === 1 ? "repository" : "repositories"}.`,
                  );
                }}
              />
            )}
          </Result>
          {!selection.data && (
            <Button
              type="button"
              variant="outline"
              className="justify-self-start"
              onClick={() => setEditing(false)}
            >
              Back to repositories
            </Button>
          )}
        </>
      ) : (
        <>
          <label className="grid gap-1">
            Find a repository
            <Input
              type="search"
              maxLength={200}
              value={search}
              onChange={(event) => {
                setSearch(event.target.value);
                setPages([]);
              }}
            />
          </label>
          <Result state={result} timingLabel="Repositories for properties">
            {({ repositories, next }) => {
              const remaining = selectionLimit - ids.length;
              const unchecked = repositories.filter(
                (repo) => selected[repo.id] === undefined,
              );
              return (
                <>
                  <fieldset className="grid max-h-64 gap-2 overflow-auto rounded-md border p-3">
                    <legend className="font-medium">Repositories</legend>
                    {!repositories.length && (
                      <p>No repositories match the search.</p>
                    )}
                    {repositories.map((repo) => (
                      <label
                        className="flex items-center gap-2 break-all"
                        key={repo.id}
                      >
                        <input
                          type="checkbox"
                          checked={selected[repo.id] !== undefined}
                          disabled={
                            remaining === 0 && selected[repo.id] === undefined
                          }
                          onChange={(event) =>
                            toggle(repo, event.target.checked)
                          }
                        />
                        {repo.name}
                      </label>
                    ))}
                  </fieldset>
                  <div className="flex flex-wrap gap-2">
                    <Button
                      type="button"
                      variant="outline"
                      disabled={
                        !unchecked.length || unchecked.length > remaining
                      }
                      onClick={() => {
                        setNotice("");
                        setSelected((current) => ({
                          ...current,
                          ...Object.fromEntries(
                            unchecked.map((repo) => [repo.id, repo.name]),
                          ),
                        }));
                      }}
                    >
                      Select this page
                    </Button>
                    {after && (
                      <Button
                        type="button"
                        variant="outline"
                        onClick={() =>
                          setPages((current) => current.slice(0, -1))
                        }
                      >
                        Previous page
                      </Button>
                    )}
                    {next && (
                      <Button
                        type="button"
                        variant="outline"
                        onClick={() =>
                          setPages((current) => [...current, next])
                        }
                      >
                        Next page
                      </Button>
                    )}
                  </div>
                </>
              );
            }}
          </Result>
          <div className="flex flex-wrap items-center gap-2">
            <Button
              type="button"
              disabled={!ids.length}
              onClick={() => {
                setNotice("");
                setEditing(true);
              }}
            >
              Edit properties
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={!ids.length}
              onClick={() => setSelected({})}
            >
              Clear selection
            </Button>
            <p className="text-sm" aria-live="polite">
              {ids.length} of {selectionLimit} selected
            </p>
          </div>
        </>
      )}
    </div>
  );
}

function SelectionEditor({
  view,
  path,
  csrf,
  reload,
  cancel,
  saved,
}: {
  view: SelectionView;
  path: string;
  csrf: string;
  reload: () => void;
  cancel: () => void;
  saved: (updated: number) => void;
}) {
  const [edits, setEdits] = useState<Record<string, Edit>>({});
  const [submission, setSubmission] = useState<Submission>();
  const mutation = useMutation(csrf, null);
  const draft = Object.entries(edits);
  const ready =
    draft.length > 0 &&
    draft.every(([, edit]) => edit.kind === "default" || edit.value !== null);
  const locked = !!submission || mutation.pending;

  function change(id: string, edit?: Edit) {
    setEdits((current) => {
      const next = { ...current };
      if (edit) next[id] = edit;
      else delete next[id];
      return next;
    });
  }

  async function submit() {
    if (!ready || mutation.pending) return;
    const input = submission ?? {
      request_id: crypto.randomUUID(),
      issued_at_ms: Date.now(),
      schema_version: view.schema.version,
      repositories: view.repositories,
      changes: Object.fromEntries(
        draft.map(([id, edit]) => [
          id,
          edit.kind === "set" ? edit.value : null,
        ]),
      ),
    };
    // A lost reply must be retried with the captured targets and patch.
    setSubmission(input);
    const result = await mutation.run<{ updated: number }>(path, "PUT", input);
    if (result) saved(result.updated);
  }

  return (
    <form
      id="repository-property-values"
      className="grid gap-3 rounded-md border p-4"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      {!view.schema.definitions.length && (
        <p>No repository properties have been defined.</p>
      )}
      <fieldset disabled={locked} className="grid gap-3">
        {view.schema.definitions.map((definition) => {
          const edit = edits[definition.id];
          const current = view.effective[definition.id];
          const summary =
            current?.kind === "mixed"
              ? "Mixed values"
              : current?.value == null
                ? "Unset"
                : display(current.value);
          return (
            <div
              className="grid gap-2 rounded-md border p-3"
              key={definition.id}
            >
              <p className="break-words">
                <strong>{definition.name}</strong> · {summary}
              </p>
              <label className="grid gap-1">
                Action for {definition.name}
                <select
                  className={control}
                  value={edit?.kind ?? "keep"}
                  onChange={(event) => {
                    const next = event.target.value;
                    change(
                      definition.id,
                      next === "set"
                        ? { kind: "set", value: null }
                        : next === "default"
                          ? { kind: "default" }
                          : undefined,
                    );
                  }}
                >
                  <option value="keep">Keep current values</option>
                  <option value="set">Set override</option>
                  <option value="default">Use default</option>
                </select>
              </label>
              {edit?.kind === "set" && (
                <PropertyInput
                  label={definition.name}
                  definition={definition}
                  value={edit.value}
                  unset="Choose a value"
                  onChange={(value) =>
                    change(definition.id, { kind: "set", value })
                  }
                />
              )}
            </div>
          );
        })}
      </fieldset>
      <div className="flex flex-wrap gap-2">
        <Button
          type="submit"
          disabled={!ready || mutation.pending || mutation.conflict}
        >
          {submission && mutation.error
            ? "Retry save"
            : "Save repository values"}
        </Button>
        <Button
          type="button"
          variant="outline"
          disabled={mutation.pending}
          onClick={cancel}
        >
          Cancel
        </Button>
        {mutation.conflict && (
          <Button type="button" variant="outline" onClick={reload}>
            Reload values and restart edit
          </Button>
        )}
        {!!submission &&
          (mutation.status === 400 || mutation.status === 422) && (
            <Button
              type="button"
              variant="outline"
              onClick={() => {
                setSubmission(undefined);
                mutation.reset();
              }}
            >
              Edit changes
            </Button>
          )}
      </div>
      <Failure message={mutation.error} />
    </form>
  );
}
