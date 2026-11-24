import { useId, useState } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import type { OrganizationView } from "@/lib/contracts";
import { navigate, useRequest, type Repository } from "@/lib/api";
import { inboxHref, kinds, reasons, views, type Criteria } from "./filters";
import { AuthorField } from "./author-field";

const path = "/api/notifications/filters";
const limit = 15;
const utf8 = new TextEncoder();
type Filter = { id: string; name: string; criteria: Criteria };
type Collection = { revision: number; items: Filter[] };
type Choice = { value: string; label: string };
type Change = { kind: "save"; filter: Filter } | { kind: "delete"; id: string };

export function SavedFilters({
  csrf,
  criteria,
  preset,
  repositories,
}: {
  csrf: string;
  criteria: Criteria;
  preset: string | null;
  repositories: Repository[];
}) {
  const resource = useRequest<Collection>(path);
  const [updated, setUpdated] = useState<Collection>();
  const [open, setOpen] = useState(false);
  const collection =
    updated && updated.revision >= (resource.data?.revision ?? 0)
      ? updated
      : resource.data;
  const selected = collection?.items.find(
    (item) =>
      item.id === preset && inboxHref(item.criteria) === inboxHref(criteria),
  );
  return (
    <div className="flex flex-wrap items-center gap-2">
      {Boolean(collection?.items.length) && (
        <FieldSelect
          aria-label="Saved filter"
          className="max-w-full"
          value={selected?.id ?? ""}
          onValueChange={(value) => {
            const item = collection?.items.find((item) => item.id === value);
            if (item) navigate(inboxHref(item.criteria, item.id));
          }}
          options={[
            { value: "", label: "Saved filters" },
            ...(collection?.items ?? []).map(({ id, name }) => ({
              value: id,
              label: name,
            })),
          ]}
        />
      )}
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogTrigger asChild>
          <Button variant="outline" size="sm">
            Manage filters
          </Button>
        </DialogTrigger>
        <DialogContent className="max-h-[85dvh] overflow-y-auto sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>Saved notification filters</DialogTitle>
            <DialogDescription>
              Save inbox views to use wherever you sign in.
            </DialogDescription>
          </DialogHeader>
          {open && (
            <Result
              state={
                collection
                  ? { loading: false, data: collection, retry: resource.retry }
                  : resource
              }
            >
              {(data) => (
                <FilterManager
                  collection={data}
                  criteria={criteria}
                  csrf={csrf}
                  repositories={repositories}
                  onChanged={setUpdated}
                  onReload={() => {
                    setUpdated(undefined);
                    resource.retry();
                  }}
                  onApply={(item) => {
                    setOpen(false);
                    navigate(inboxHref(item.criteria, item.id));
                  }}
                />
              )}
            </Result>
          )}
        </DialogContent>
      </Dialog>
    </div>
  );
}

function FilterManager({
  collection,
  criteria,
  csrf,
  repositories,
  onChanged,
  onReload,
  onApply,
}: {
  collection: Collection;
  criteria: Criteria;
  csrf: string;
  repositories: Repository[];
  onChanged: (collection: Collection) => void;
  onReload: () => void;
  onApply: (filter: Filter) => void;
}) {
  const [draft, setDraft] = useState<Filter>();
  const [deleting, setDeleting] = useState(false);
  const [notice, setNotice] = useState("");
  const mutation = useMutation(csrf, null);
  const name = draft?.name.trim() ?? "";
  const nameTooLong = utf8.encode(name).length > 128;
  const atLimit = collection.items.length >= limit;
  const isExisting = collection.items.some((item) => item.id === draft?.id);
  const changedElsewhere = mutation.status === 409;
  function begin(next?: Filter) {
    mutation.reset();
    setDeleting(false);
    setNotice("");
    setDraft(next);
  }
  async function submit(change: Change) {
    const current = await mutation.run<Collection>(path, "PUT", {
      revision: collection.revision,
      change,
    });
    if (!current) return;
    onChanged(current);
    begin();
    if (change.kind === "save") onApply(change.filter);
    else setNotice("Filter deleted.");
  }
  if (!draft)
    return (
      <div className="grid gap-3">
        {collection.items.length > 0 && (
          <ul className="divide-y rounded-lg border">
            {collection.items.map((item) => (
              <li
                key={item.id}
                className="flex items-center justify-between gap-2 px-3 py-2"
              >
                <Button
                  variant="link"
                  className="h-auto min-w-0 wrap-anywhere whitespace-normal px-0 text-left"
                  onClick={() => onApply(item)}
                >
                  {item.name}
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  aria-label={`Edit ${item.name}`}
                  onClick={() => begin(item)}
                >
                  Edit
                </Button>
              </li>
            ))}
          </ul>
        )}
        {notice && (
          <p role="status" className="text-sm">
            {notice}
          </p>
        )}
        {atLimit && (
          <p className="text-sm text-muted-foreground">
            You can save up to 15 filters.
          </p>
        )}
        <Button
          className="justify-self-start"
          disabled={atLimit}
          onClick={() => begin({ id: crypto.randomUUID(), name: "", criteria })}
        >
          New filter
        </Button>
      </div>
    );

  return (
    <form
      className="grid gap-3"
      onSubmit={(event) => {
        event.preventDefault();
        if (changedElsewhere || mutation.pending) return;
        if (deleting) void submit({ kind: "delete", id: draft.id });
        else if (name && !nameTooLong)
          void submit({ kind: "save", filter: { ...draft, name } });
      }}
    >
      {deleting ? (
        <p className="text-sm">
          Delete “{draft.name}” from your saved filters?
        </p>
      ) : (
        <fieldset
          disabled={mutation.pending || changedElsewhere}
          className="grid gap-3"
        >
          <label className="grid gap-1.5 text-sm font-medium">
            Filter name
            <Input
              name="filter-name"
              required
              autoFocus
              value={draft.name}
              maxLength={128}
              onChange={(event) =>
                setDraft({ ...draft, name: event.target.value })
              }
            />
          </label>
          {nameTooLong && (
            <p className="text-sm text-muted-foreground">
              Enter a name of up to 128 bytes.
            </p>
          )}
          <FilterFields
            repositories={repositories}
            criteria={draft.criteria}
            onChange={(criteria) => setDraft({ ...draft, criteria })}
          />
        </fieldset>
      )}
      {mutation.error && (
        <p role="alert" className="text-sm">
          {mutation.error}
        </p>
      )}
      <div className="flex flex-wrap items-center justify-end gap-2">
        {isExisting && !deleting && !changedElsewhere && (
          <Button
            type="button"
            variant="ghost"
            className="mr-auto"
            disabled={mutation.pending}
            onClick={() => {
              mutation.reset();
              setDeleting(true);
            }}
          >
            Delete filter
          </Button>
        )}
        <Button
          type="button"
          variant="outline"
          disabled={mutation.pending}
          onClick={() =>
            deleting ? (mutation.reset(), setDeleting(false)) : begin()
          }
        >
          Cancel
        </Button>
        {changedElsewhere ? (
          <Button
            type="button"
            onClick={() => {
              begin();
              onReload();
            }}
          >
            Reload filters
          </Button>
        ) : (
          <Button
            type="submit"
            variant={deleting ? "destructive" : "default"}
            disabled={mutation.pending || (!deleting && (!name || nameTooLong))}
          >
            {deleting ? "Confirm deletion" : "Save filter"}
          </Button>
        )}
      </div>
    </form>
  );
}

function FilterFields({
  criteria,
  repositories,
  onChange,
}: {
  criteria: Criteria;
  repositories: Repository[];
  onChange: (criteria: Criteria) => void;
}) {
  const fieldPrefix = useId();
  const organizations = useRequest<{ items: OrganizationView[] }>(
    "/api/organizations",
  );
  const labels = new Map(
    (organizations.data?.items ?? []).map(({ organization }) => [
      organization.id,
      organization.name,
    ]),
  );
  const organizationChoices = new Map<string, string>();
  for (const repo of repositories)
    if (repo.organization_id)
      organizationChoices.set(
        repo.organization_id,
        labels.get(repo.organization_id) ?? repo.owner,
      );
  const fields: {
    key: Exclude<keyof Criteria, "author">;
    label: string;
    choices: Choice[];
    empty?: string;
  }[] = [
    {
      key: "view",
      label: "View",
      choices: Object.entries(views).map(([value, { label }]) => ({
        value,
        label,
      })),
    },
    {
      key: "organization",
      label: "Organization",
      empty: "All organizations",
      choices: Array.from(organizationChoices, ([value, label]) => ({
        value,
        label,
      })),
    },
    {
      key: "repository",
      label: "Repository",
      empty: "All repositories",
      choices: repositories.map(({ id, owner, name }) => ({
        value: id,
        label: `${owner}/${name}`,
      })),
    },
    {
      key: "reason",
      label: "Reason",
      empty: "All reasons",
      choices: Object.entries(reasons).map(([value, { label }]) => ({
        value,
        label,
      })),
    },
    {
      key: "kind",
      label: "Notification type",
      empty: "All types",
      choices: Object.entries(kinds).map(([value, label]) => ({
        value,
        label,
      })),
    },
  ];
  return (
    <div className="grid gap-3 sm:grid-cols-2">
      {fields.map(({ key, label, choices, empty }) => {
        const value = criteria[key] ?? "";
        const fieldId = `${fieldPrefix}-${key}`;
        const options = empty ? [{ value: "", label: empty }] : [];
        if (value && !choices.some((choice) => choice.value === value)) {
          options.push({ value, label: `Unavailable ${label.toLowerCase()}` });
        }
        return (
          <div key={key} className="grid min-w-0 gap-1.5 text-sm font-medium">
            <label htmlFor={fieldId}>{label}</label>
            <FieldSelect
              id={fieldId}
              className="w-full"
              name={key}
              value={value}
              onValueChange={(value) =>
                onChange({
                  ...criteria,
                  [key]: value || undefined,
                })
              }
              options={[...options, ...choices]}
            />
          </div>
        );
      })}
      <AuthorField
        author={criteria.author}
        onChange={(author) => onChange({ ...criteria, author })}
      />
    </div>
  );
}
