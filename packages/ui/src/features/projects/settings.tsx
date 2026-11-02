import { ProjectDraftReview } from "./review";
import { useId } from "react";
import { PlusIcon, Trash2Icon } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "@/components/ui/dialog";
import {
  Field,
  FieldGroup,
  FieldLabel,
  FieldDescription,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { FieldSelect } from "@/components/ui/field-select";
import { Checkbox } from "@/components/ui/checkbox";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useProjectCommands, CommandError } from "./commands";
import type { Project, Field as ProjectField, Layout } from "./model";
import { useProjectDraft } from "./drafts";
export function ProjectSettings({
  target,
  project,
  onClose,
}: {
  target: import("./model").ProjectTarget;
  project: Project;
  onClose: () => void;
}) {
  const saved = useProjectDraft(
    { target, project: project.id },
    {
      kind: "settings",
      definition: project.definition,
      statuses: project.definition.statuses.join("\n"),
      archived: project.archived,
      version: project.version,
    },
  );
  const { definition: draft, statuses, archived, version } = saved.value;
  const setDraft = (definition: Project["definition"]) =>
    saved.update({ definition });
  const setStatuses = (statuses: string) => saved.update({ statuses });
  const setArchived = (archived: boolean) => saved.update({ archived });
  const setVersion = (version: number) => saved.update({ version });
  const commands = useProjectCommands();
  const id = useId();
  function field(index: number, value: ProjectField) {
    setDraft({
      ...draft,
      fields: draft.fields.map((f, i) => (i === index ? value : f)),
    });
  }
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !commands.locked) onClose();
      }}
    >
      <DialogContent
        className="project-settings-dialog"
        onEscapeKeyDown={(e) => {
          if (commands.locked) e.preventDefault();
        }}
        onInteractOutside={(e) => {
          if (commands.locked) e.preventDefault();
        }}
      >
        <DialogHeader>
          <DialogTitle>Project settings</DialogTitle>
          <DialogDescription>
            Manage the workflow, fields, and views for {project.definition.name}
            .
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex min-h-0 flex-col gap-5"
          onSubmit={async (e) => {
            e.preventDefault();
            const command = {
              kind: "edit",
              project: project.id,
              version,
              definition: {
                ...draft,
                name: draft.name.trim(),
                statuses: statuses
                  .split("\n")
                  .map((s) => s.trim())
                  .filter(Boolean),
              },
              archived,
            } as const;
            const checkpoint = saved.checkpoint();
            if (!checkpoint) return;
            const result =
              "repo" in target
                ? await commands.submit(target.repo, command, checkpoint)
                : await commands.submitOrganization(
                    target.organization,
                    command,
                    checkpoint,
                  );
            if (result?.outcome === "applied") onClose();
          }}
        >
          <fieldset disabled={commands.locked} className="project-form-scroll">
            <Tabs defaultValue="general">
              <TabsList>
                <TabsTrigger value="general">General</TabsTrigger>
                <TabsTrigger value="workflow">Workflow</TabsTrigger>
                <TabsTrigger value="fields">Fields</TabsTrigger>
                <TabsTrigger value="views">Views</TabsTrigger>
              </TabsList>
              <TabsContent value="general">
                <FieldGroup>
                  <Field>
                    <FieldLabel htmlFor={`${id}-name`}>Project name</FieldLabel>
                    <Input
                      id={`${id}-name`}
                      required
                      maxLength={100}
                      value={draft.name}
                      onChange={(e) =>
                        setDraft({ ...draft, name: e.target.value })
                      }
                    />
                  </Field>
                  <Field>
                    <FieldLabel htmlFor={`${id}-description`}>
                      Short description
                    </FieldLabel>
                    <Textarea
                      id={`${id}-description`}
                      maxLength={1000}
                      value={draft.description}
                      onChange={(e) =>
                        setDraft({ ...draft, description: e.target.value })
                      }
                    />
                  </Field>
                  <Field>
                    <FieldLabel htmlFor={`${id}-readme`}>
                      Project README
                    </FieldLabel>
                    <Textarea
                      id={`${id}-readme`}
                      rows={5}
                      maxLength={32768}
                      value={draft.readme}
                      onChange={(e) =>
                        setDraft({ ...draft, readme: e.target.value })
                      }
                    />
                    <FieldDescription>
                      Markdown is supported in the project overview.
                    </FieldDescription>
                  </Field>
                  {"repo" in target && (
                    <>
                      <Field orientation="horizontal">
                        <Checkbox
                          id={`${id}-template`}
                          checked={draft.is_template}
                          onCheckedChange={(v) =>
                            setDraft({ ...draft, is_template: v === true })
                          }
                        />
                        <FieldLabel htmlFor={`${id}-template`}>
                          Make available as a project template
                        </FieldLabel>
                      </Field>
                      <FieldDescription>
                        A template copies settings, fields, and views. Issue and
                        PR memberships remain in their original project.
                      </FieldDescription>
                    </>
                  )}
                  <Field orientation="horizontal">
                    <Checkbox
                      id={`${id}-archived`}
                      checked={archived}
                      onCheckedChange={(v) => setArchived(v === true)}
                    />
                    <FieldLabel htmlFor={`${id}-archived`}>
                      Archive project
                    </FieldLabel>
                  </Field>
                  <FieldDescription>
                    Archived projects retain their items and become read-only
                    until reopened.
                  </FieldDescription>
                  <FieldDescription>
                    {"repo" in target
                      ? "Access follows this repository’s members and SSO group grants."
                      : "Organization members can view this project. Its creator and organization owners manage planning. Repository permissions still protect linked work."}
                  </FieldDescription>
                </FieldGroup>
              </TabsContent>
              <TabsContent value="workflow">
                <FieldGroup>
                  <Field>
                    <FieldLabel htmlFor={`${id}-statuses`}>
                      Workflow statuses
                    </FieldLabel>
                    <Textarea
                      id={`${id}-statuses`}
                      rows={8}
                      required
                      value={statuses}
                      onChange={(e) => setStatuses(e.target.value)}
                    />
                    <FieldDescription>
                      One status per line, up to 12. Move items before removing
                      a status. Project workflow status does not close issues or
                      merge PRs.
                    </FieldDescription>
                  </Field>
                </FieldGroup>
              </TabsContent>
              <TabsContent value="fields">
                <FieldGroup>
                  {draft.fields.map((f, i) => (
                    <Field key={f.id}>
                      <div className="flex items-end gap-2">
                        <Field>
                          <FieldLabel htmlFor={`${id}-field-${f.id}`}>
                            Field name
                          </FieldLabel>
                          <Input
                            id={`${id}-field-${f.id}`}
                            maxLength={60}
                            required
                            value={f.name}
                            onChange={(e) =>
                              field(i, { ...f, name: e.target.value })
                            }
                          />
                        </Field>
                        <Field>
                          <FieldLabel htmlFor={`${id}-type-${f.id}`}>
                            Type
                          </FieldLabel>
                          <FieldSelect
                            label="Field type"
                            id={`${id}-type-${f.id}`}
                            disabled={project.definition.fields.some(
                              (old) => old.id === f.id,
                            )}
                            value={f.definition.kind}
                            onValueChange={(type) =>
                              field(i, {
                                ...f,
                                definition:
                                  type === "select"
                                    ? { kind: "select", options: ["Option 1"] }
                                    : {
                                        kind: type as
                                          | "text"
                                          | "number"
                                          | "date",
                                      },
                              })
                            }
                            options={{
                              text: "Text",
                              number: "Number",
                              date: "Date",
                              select: "Single select",
                            }}
                          />
                        </Field>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon"
                          disabled={project.definition.fields.some(
                            (old) => old.id === f.id,
                          )}
                          aria-label={`Remove ${f.name || "field"}`}
                          onClick={() =>
                            setDraft({
                              ...draft,
                              fields: draft.fields.filter(
                                (field) => field.id !== f.id,
                              ),
                            })
                          }
                        >
                          <Trash2Icon />
                        </Button>
                      </div>
                      {f.definition.kind === "select" && (
                        <Field>
                          <FieldLabel htmlFor={`${id}-options-${f.id}`}>
                            Options (one per line)
                          </FieldLabel>
                          <Textarea
                            id={`${id}-options-${f.id}`}
                            value={f.definition.options.join("\n")}
                            onChange={(e) =>
                              field(i, {
                                ...f,
                                definition: {
                                  kind: "select",
                                  options: e.target.value.split("\n"),
                                },
                              })
                            }
                          />
                        </Field>
                      )}
                    </Field>
                  ))}
                  <Button
                    type="button"
                    variant="outline"
                    disabled={draft.fields.length >= 20}
                    onClick={() =>
                      setDraft({
                        ...draft,
                        fields: [
                          ...draft.fields,
                          {
                            id: crypto.randomUUID(),
                            name: "New field",
                            definition: { kind: "text" },
                          },
                        ],
                      })
                    }
                  >
                    <PlusIcon data-icon="inline-start" />
                    Add field
                  </Button>
                  <FieldDescription>
                    Up to 20 typed fields. Existing field types and values are
                    preserved.
                  </FieldDescription>
                </FieldGroup>
              </TabsContent>
              <TabsContent value="views">
                <FieldGroup>
                  {draft.views.map((view, i) => (
                    <Field key={view.id}>
                      <div className="project-field-grid">
                        <Field>
                          <FieldLabel htmlFor={`${id}-view-${view.id}`}>
                            View name
                          </FieldLabel>
                          <Input
                            id={`${id}-view-${view.id}`}
                            maxLength={60}
                            required
                            value={view.name}
                            onChange={(e) =>
                              setDraft({
                                ...draft,
                                views: draft.views.map((v, n) =>
                                  n === i ? { ...v, name: e.target.value } : v,
                                ),
                              })
                            }
                          />
                        </Field>
                        <Field>
                          <FieldLabel htmlFor={`${id}-layout-${view.id}`}>
                            Layout
                          </FieldLabel>
                          <FieldSelect
                            label="View layout"
                            id={`${id}-layout-${view.id}`}
                            value={view.layout}
                            onValueChange={(layout) =>
                              setDraft({
                                ...draft,
                                views: draft.views.map((v, n) =>
                                  n === i
                                    ? { ...v, layout: layout as Layout }
                                    : v,
                                ),
                              })
                            }
                            options={{
                              table: "Table",
                              board: "Board",
                              roadmap: "Roadmap",
                            }}
                          />
                        </Field>
                        {"repo" in target && (
                          <Field>
                            <FieldLabel htmlFor={`${id}-query-${view.id}`}>
                              Title filter
                            </FieldLabel>
                            <Input
                              id={`${id}-query-${view.id}`}
                              maxLength={256}
                              value={view.query}
                              onChange={(e) =>
                                setDraft({
                                  ...draft,
                                  views: draft.views.map((v, n) =>
                                    n === i
                                      ? { ...v, query: e.target.value }
                                      : v,
                                  ),
                                })
                              }
                            />
                          </Field>
                        )}
                        <Field>
                          <FieldLabel htmlFor={`${id}-status-${view.id}`}>
                            Status filter
                          </FieldLabel>
                          <FieldSelect
                            label="Saved view status filter"
                            id={`${id}-status-${view.id}`}
                            value={
                              view.status === null
                                ? "all"
                                : `status:${view.status}`
                            }
                            onValueChange={(value) =>
                              setDraft({
                                ...draft,
                                views: draft.views.map((v, n) =>
                                  n === i
                                    ? {
                                        ...v,
                                        status:
                                          value === "all"
                                            ? null
                                            : value.slice(7),
                                      }
                                    : v,
                                ),
                              })
                            }
                            options={{
                              all: "All statuses",
                              ...Object.fromEntries(
                                draft.statuses.map((status) => [
                                  `status:${status}`,
                                  status,
                                ]),
                              ),
                            }}
                          />
                        </Field>
                      </div>
                      {"repo" in target && (
                        <Field orientation="horizontal">
                          <Checkbox
                            id={`${id}-closed-${view.id}`}
                            checked={view.hide_closed}
                            onCheckedChange={(v) =>
                              setDraft({
                                ...draft,
                                views: draft.views.map((value, n) =>
                                  n === i
                                    ? { ...value, hide_closed: v === true }
                                    : value,
                                ),
                              })
                            }
                          />
                          <FieldLabel htmlFor={`${id}-closed-${view.id}`}>
                            Hide closed issues and PRs
                          </FieldLabel>
                        </Field>
                      )}
                      <Button
                        type="button"
                        variant="ghost"
                        disabled={draft.views.length === 1}
                        onClick={() =>
                          setDraft({
                            ...draft,
                            views: draft.views.filter((v) => v.id !== view.id),
                          })
                        }
                      >
                        Remove view
                      </Button>
                    </Field>
                  ))}
                  <Button
                    type="button"
                    variant="outline"
                    disabled={draft.views.length >= 12}
                    onClick={() =>
                      setDraft({
                        ...draft,
                        views: [
                          ...draft.views,
                          {
                            id: crypto.randomUUID(),
                            name: "New view",
                            layout: "table",
                            query: "",
                            status: null,
                            hide_closed: false,
                          },
                        ],
                      })
                    }
                  >
                    <PlusIcon data-icon="inline-start" />
                    Add view
                  </Button>
                </FieldGroup>
              </TabsContent>
            </Tabs>
          </fieldset>
          {saved.feedback}
          <CommandError />
          <ProjectDraftReview
            target={target}
            project={project.id}
            onReviewed={(latest) => setVersion(latest.version)}
          />
          <DialogFooter>
            <Button type="submit" disabled={commands.locked}>
              Save project settings
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
