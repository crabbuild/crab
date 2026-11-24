import { useState } from "react";
import { IssueTypeBadge } from "@/components/discussion/issue-type-badge";
import { useMutation } from "@/components/discussion/mutations";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { FieldSelect } from "@/components/ui/field-select";
import { Input } from "@/components/ui/input";
import { useRequest } from "@/lib/api";
import type {
  IssueType,
  IssueTypeColor,
  IssueTypeSchema,
} from "@/lib/contracts";
import { MutationError } from "./feedback";

const issueTypeColors: Record<IssueTypeColor, string> = {
  gray: "Gray",
  blue: "Blue",
  green: "Green",
  yellow: "Yellow",
  orange: "Orange",
  red: "Red",
  purple: "Purple",
  pink: "Pink",
};

export function OrganizationIssueTypes({
  organization,
  csrf,
}: {
  organization: string;
  csrf: string;
}) {
  const endpoint = `/api/organizations/${organization}/issue-types`;
  const resource = useRequest<IssueTypeSchema>(endpoint);
  const [saved, setSaved] = useState<IssueTypeSchema>();
  const [draft, setDraft] = useState<IssueType>();
  const [deleting, setDeleting] = useState<IssueType>();
  const mutation = useMutation(csrf, null);
  return (
    <section className="grid gap-4" aria-label="Organization issue types">
      <h2 className="text-lg font-semibold">Issue types</h2>
      <Result state={resource} timingLabel="Issue types">
        {(loaded) => {
          const schema = saved ?? loaded;
          const editing = draft || deleting;
          const locked = mutation.pending || mutation.conflict;
          async function save(definitions: IssueType[]) {
            const response = await mutation.run<IssueTypeSchema>(
              endpoint,
              "PUT",
              {
                expected_version: schema.version,
                definitions,
              },
            );
            if (response) {
              setSaved(response);
              setDraft(undefined);
              setDeleting(undefined);
            }
          }
          return (
            <>
              <MutationError error={mutation.error} />
              {mutation.conflict && (
                <Button
                  type="button"
                  variant="outline"
                  className="justify-self-start"
                  onClick={() => {
                    mutation.reset();
                    setSaved(undefined);
                    setDraft(undefined);
                    setDeleting(undefined);
                    resource.retry();
                  }}
                >
                  Reload issue types
                </Button>
              )}
              {schema.definitions.length > 0 && (
                <ul className="divide-y rounded-lg border">
                  {schema.definitions.map((type) => (
                    <li
                      key={type.id}
                      className="flex flex-wrap items-center justify-between gap-3 p-3"
                    >
                      <div className="min-w-0">
                        <IssueTypeBadge type={type} />
                        {!type.enabled && (
                          <span className="ms-2 text-sm text-muted-foreground">
                            Disabled
                          </span>
                        )}
                        {type.description && (
                          <p className="mt-1 break-words text-sm text-muted-foreground">
                            {type.description}
                          </p>
                        )}
                      </div>
                      <div className="flex flex-wrap gap-2">
                        <Button
                          variant="outline"
                          size="sm"
                          aria-label={`Edit ${type.name}`}
                          disabled={locked || !!editing}
                          onClick={() => {
                            mutation.reset();
                            setDraft(type);
                          }}
                        >
                          Edit
                        </Button>
                        <Button
                          variant="outline"
                          size="sm"
                          aria-label={`${type.enabled ? "Disable" : "Enable"} ${type.name}`}
                          disabled={locked || !!editing}
                          onClick={() =>
                            void save(
                              schema.definitions.map((item) =>
                                item.id === type.id
                                  ? { ...item, enabled: !item.enabled }
                                  : item,
                              ),
                            )
                          }
                        >
                          {type.enabled ? "Disable" : "Enable"}
                        </Button>
                        <Button
                          variant="outline"
                          size="sm"
                          aria-label={`Delete ${type.name}`}
                          disabled={locked || !!editing}
                          onClick={() => {
                            mutation.reset();
                            setDeleting(type);
                          }}
                        >
                          Delete
                        </Button>
                      </div>
                    </li>
                  ))}
                </ul>
              )}
              {draft ? (
                <form
                  className="grid gap-4 rounded-lg border p-4"
                  onSubmit={(event) => {
                    event.preventDefault();
                    const updated = { ...draft, name: draft.name.trim() };
                    void save(
                      schema.definitions.some((item) => item.id === draft.id)
                        ? schema.definitions.map((item) =>
                            item.id === draft.id ? updated : item,
                          )
                        : [...schema.definitions, updated],
                    );
                  }}
                >
                  <FieldGroup>
                    <Field>
                      <FieldLabel htmlFor="issue-type-name">Name</FieldLabel>
                      <Input
                        id="issue-type-name"
                        maxLength={75}
                        required
                        value={draft.name}
                        onChange={(event) =>
                          setDraft({ ...draft, name: event.target.value })
                        }
                      />
                    </Field>
                    <Field>
                      <FieldLabel htmlFor="issue-type-description">
                        Description
                      </FieldLabel>
                      <Input
                        id="issue-type-description"
                        maxLength={256}
                        value={draft.description}
                        onChange={(event) =>
                          setDraft({
                            ...draft,
                            description: event.target.value,
                          })
                        }
                      />
                    </Field>
                    <Field>
                      <FieldLabel htmlFor="issue-type-color">Color</FieldLabel>
                      <FieldSelect
                        label="Color"
                        id="issue-type-color"
                        options={issueTypeColors}
                        value={draft.color}
                        onValueChange={(color) =>
                          setDraft({
                            ...draft,
                            color: color as IssueType["color"],
                          })
                        }
                      />
                    </Field>
                  </FieldGroup>
                  <div className="flex gap-2">
                    <Button
                      type="submit"
                      disabled={locked || !draft.name.trim()}
                    >
                      Save type
                    </Button>
                    <Button
                      type="button"
                      variant="outline"
                      disabled={mutation.pending}
                      onClick={() => setDraft(undefined)}
                    >
                      Cancel
                    </Button>
                  </div>
                </form>
              ) : deleting ? (
                <div className="grid gap-3 rounded-lg border p-4">
                  <p className="text-sm">
                    Delete {deleting.name} and remove it from issues that use
                    it?
                  </p>
                  <div className="flex flex-wrap gap-2">
                    <Button
                      variant="destructive"
                      disabled={locked}
                      onClick={() =>
                        void save(
                          schema.definitions.filter(
                            (item) => item.id !== deleting.id,
                          ),
                        )
                      }
                    >
                      Delete type
                    </Button>
                    <Button
                      variant="outline"
                      disabled={mutation.pending}
                      onClick={() => setDeleting(undefined)}
                    >
                      Cancel
                    </Button>
                  </div>
                </div>
              ) : (
                <Button
                  variant="outline"
                  className="justify-self-start"
                  disabled={locked || schema.definitions.length >= 25}
                  onClick={() => {
                    mutation.reset();
                    setDraft({
                      id: crypto.randomUUID(),
                      name: "",
                      description: "",
                      color: "gray",
                      enabled: true,
                    });
                  }}
                >
                  Add issue type
                </Button>
              )}
            </>
          );
        }}
      </Result>
    </section>
  );
}
