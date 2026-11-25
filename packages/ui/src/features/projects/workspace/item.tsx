import { useId, useState } from "react";
import { CircleDotIcon, GitPullRequestIcon } from "lucide-react";
import { useRequest } from "@/lib/api";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { DatePicker } from "@/components/ui/date-picker";
import { Textarea } from "@/components/ui/textarea";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "@/components/ui/dialog";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { CommandError, useProjectCommands } from "../commands";
import {
  blankItem,
  priorities,
  projectEndpoint,
  type Row,
  type Work,
  type Item,
} from "../model";
import { useProjectDraft } from "../drafts";
import { ProjectItemReview } from "../review";
import { FieldSelect } from "@/components/ui/field-select";
import type { Detail } from "./model";

export function WorkspaceItemEditor({
  organization,
  detail,
  row,
  draftItem,
  initialItem,
  initialIssueType,
  onClose,
}: {
  organization: string;
  detail: Detail;
  row?: Row;
  draftItem?: string;
  initialItem?: Item;
  initialIssueType?: string | null;
  onClose: () => void;
}) {
  const writable = detail.repositories.filter((repo) => repo.can_create_issue);
  const issueDefault = detail.default_repository_configured
    ? (writable.find((repo) => repo.id === detail.default_repository)?.id ?? "")
    : (writable[0]?.id ?? "");
  const typeChoices = detail.issue_types ?? [];
  const saved = useProjectDraft(
    {
      target: { organization },
      project: detail.project.id,
      item: row?.item.id ?? draftItem ?? initialItem?.id,
    },
    {
      kind: "item",
      item:
        row?.item ??
        initialItem ??
        blankItem(detail.project.definition.statuses[0]),
      mode: row
        ? row.work
          ? "existing"
          : "draft"
        : initialIssueType !== undefined
          ? "issue"
          : detail.repositories.length
            ? "existing"
            : "draft",
      repository:
        row?.source_repository ??
        (initialIssueType !== undefined
          ? issueDefault
          : detail.repositories[0]?.id) ??
        "",
      issueType: initialIssueType,
      parent: "",
      work: row?.work ?? null,
      version: row?.version ?? 0,
      definition: detail.project.definition,
      existing: !!row,
      restricted: !!row?.restricted,
    },
  );
  const {
    item,
    mode,
    repository,
    work: picked,
    issueType,
    version,
    definition,
    existing: wasExisting,
    restricted,
  } = saved.value;
  const [issueRepositoryChoice, setIssueRepositoryChoice] = useState(() =>
    mode === "issue" ? repository : undefined,
  );
  const canWriteIssue = writable.some((repo) => repo.id === repository);
  const canConvertDraft =
    wasExisting &&
    item.subject === null &&
    (writable.length > 0 || mode === "issue");
  const setItem = (item: Item) => saved.update({ item });
  const setMode = (mode: string) => {
    if (mode === "existing" || mode === "draft") saved.update({ mode });
    if (mode === "issue")
      saved.update({
        mode,
        repository:
          issueRepositoryChoice ??
          (!detail.default_repository_configured && canWriteIssue
            ? repository
            : issueDefault),
        work: null,
      });
  };
  const setRepository = (repository: string) => saved.update({ repository });
  const setPicked = (work: Work | null | undefined) =>
    saved.update({ work: work ?? null });
  const setVersion = (version: number) => saved.update({ version });
  const setDefinition = (definition: typeof detail.project.definition) =>
    saved.update({ definition });
  const [query, setQuery] = useState("");
  const id = useId();
  const commands = useProjectCommands();
  const candidates = useRequest<{ items: Work[] }>(
    mode === "existing" && !wasExisting && repository
      ? projectEndpoint({ organization }, `${detail.project.id}/candidates`, {
          repository,
          q: query,
        })
      : null,
  );
  function value(field: string, value: string) {
    const values = { ...item.values };
    if (value) values[field] = value;
    else delete values[field];
    setItem({ ...item, values });
  }
  async function remove() {
    const checkpoint = saved.checkpoint();
    if (!checkpoint) return;
    const result = await commands.submitOrganization(
      organization,
      {
        kind: "remove",
        project: detail.project.id,
        version,
        item: item.id,
      },
      checkpoint,
    );
    if (result?.outcome === "applied") onClose();
  }
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !commands.locked) onClose();
      }}
    >
      <DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>
            {wasExisting ? "Project item" : "Add work to project"}
          </DialogTitle>
          <DialogDescription>
            {detail.project.definition.name} · Planning fields apply to this
            project.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-5"
          onSubmit={async (event) => {
            event.preventDefault();
            const checkpoint = saved.checkpoint();
            if (!checkpoint) return;
            const result = await commands.submitOrganization(
              organization,
              mode === "issue"
                ? {
                    kind: "create_issue",
                    project: detail.project.id,
                    repository,
                    item: { ...item, subject: null, title: item.title.trim() },
                    type_id: issueType ?? null,
                    ...(wasExisting ? { draft_version: version } : {}),
                  }
                : {
                    kind: "item",
                    project: detail.project.id,
                    version,
                    repository: mode === "existing" ? repository : null,
                    item: {
                      ...item,
                      subject:
                        mode === "existing"
                          ? ((wasExisting ? item.subject : picked?.subject) ??
                            null)
                          : null,
                      title: mode === "existing" ? "" : item.title.trim(),
                      body: mode === "existing" ? "" : item.body,
                    },
                  },
              checkpoint,
            );
            if (
              result?.outcome === "applied" ||
              result?.outcome === "issue_already_linked"
            )
              onClose();
          }}
        >
          {restricted ? (
            <Alert>
              <AlertDescription>
                Repository access is required to edit this item. You can remove
                its membership from the project.
              </AlertDescription>
            </Alert>
          ) : (
            <fieldset disabled={commands.locked}>
              <FieldGroup>
                {!wasExisting || canConvertDraft ? (
                  <Tabs value={mode} onValueChange={setMode}>
                    <TabsList aria-label="Work source">
                      {wasExisting ? (
                        <TabsTrigger value="draft">Draft note</TabsTrigger>
                      ) : (
                        <TabsTrigger value="existing">
                          Issue or pull request
                        </TabsTrigger>
                      )}
                      {(writable.length > 0 || mode === "issue") && (
                        <TabsTrigger value="issue">
                          {wasExisting ? "Convert to issue" : "New issue"}
                        </TabsTrigger>
                      )}
                      {!wasExisting && (
                        <TabsTrigger value="draft">Draft note</TabsTrigger>
                      )}
                    </TabsList>
                    <TabsContent value="existing">
                      <FieldGroup className="pt-4">
                        <Field>
                          <FieldLabel htmlFor={`${id}-repository`}>
                            Repository
                          </FieldLabel>
                          {detail.repositories.length ? (
                            <FieldSelect
                              id={`${id}-repository`}
                              label="Source repository"
                              value={repository}
                              onValueChange={(repository) => {
                                setRepository(repository);
                                setPicked(undefined);
                              }}
                              options={Object.fromEntries(
                                detail.repositories.map((repo) => [
                                  repo.id,
                                  `${repo.owner}/${repo.name}`,
                                ]),
                              )}
                            />
                          ) : (
                            <p className="text-sm text-muted-foreground">
                              Link a repository from the project overview first,
                              or add a draft note.
                            </p>
                          )}
                        </Field>
                        {repository && (
                          <Field>
                            <FieldLabel htmlFor={`${id}-search`}>
                              Find an issue or pull request
                            </FieldLabel>
                            <Input
                              id={`${id}-search`}
                              value={query}
                              onChange={(event) => setQuery(event.target.value)}
                              maxLength={256}
                              placeholder="Search title or #number…"
                            />
                          </Field>
                        )}
                        {repository && (
                          <Result state={candidates}>
                            {({ items }) => (
                              <div className="max-h-48 overflow-y-auto">
                                {items.map((work) => (
                                  <Button
                                    className="workspace-work-choice"
                                    variant={
                                      picked?.subject.kind ===
                                        work.subject.kind &&
                                      picked.subject.number ===
                                        work.subject.number
                                        ? "secondary"
                                        : "ghost"
                                    }
                                    key={`${work.subject.kind}:${work.subject.number}`}
                                    onClick={() => setPicked(work)}
                                  >
                                    {work.subject.kind === "issue" ? (
                                      <CircleDotIcon data-icon="inline-start" />
                                    ) : (
                                      <GitPullRequestIcon data-icon="inline-start" />
                                    )}
                                    <span>{work.title}</span>
                                    <small>#{work.subject.number}</small>
                                  </Button>
                                ))}
                                {!items.length && (
                                  <p className="py-3 text-sm text-muted-foreground">
                                    No matching work in this repository.
                                  </p>
                                )}
                              </div>
                            )}
                          </Result>
                        )}
                      </FieldGroup>
                    </TabsContent>
                    <TabsContent value="issue">
                      <FieldGroup className="pt-4">
                        <Field>
                          <FieldLabel htmlFor={`${id}-issue-repository`}>
                            Repository
                          </FieldLabel>
                          <FieldSelect
                            id={`${id}-issue-repository`}
                            label="Issue repository"
                            value={canWriteIssue ? repository : "choose"}
                            onValueChange={(choice) => {
                              setIssueRepositoryChoice(choice);
                              setRepository(choice);
                            }}
                            options={{
                              choose: "Choose a repository",
                              ...Object.fromEntries(
                                writable.map((repo) => [
                                  repo.id,
                                  `${repo.owner}/${repo.name}`,
                                ]),
                              ),
                            }}
                          />
                          {detail.default_repository_configured &&
                            !issueDefault && (
                              <p className="text-sm text-muted-foreground">
                                The default repository is unavailable for issue
                                creation.
                              </p>
                            )}
                        </Field>
                        {!writable.length && (
                          <p className="text-sm text-muted-foreground">
                            Write access to a linked organization repository is
                            required.
                          </p>
                        )}
                        {(typeChoices.length > 0 || issueType != null) && (
                          <Field>
                            <FieldLabel htmlFor={`${id}-issue-type`}>
                              Issue type
                            </FieldLabel>
                            <FieldSelect
                              id={`${id}-issue-type`}
                              label="Issue type"
                              value={issueType ?? "none"}
                              options={{
                                none: "No type",
                                ...Object.fromEntries(
                                  typeChoices.map((type) => [
                                    type.id,
                                    type.name,
                                  ]),
                                ),
                                ...(issueType &&
                                !typeChoices.some(
                                  (type) => type.id === issueType,
                                )
                                  ? { [issueType]: "Unavailable type" }
                                  : {}),
                              }}
                              onValueChange={(value) =>
                                saved.update({
                                  issueType: value === "none" ? null : value,
                                })
                              }
                            />
                          </Field>
                        )}
                      </FieldGroup>
                    </TabsContent>
                    <TabsContent
                      value="draft"
                      className={wasExisting ? undefined : "pt-4"}
                    >
                      {!wasExisting && (
                        <p className="text-sm text-muted-foreground">
                          Capture work before it has an issue. Draft notes
                          belong to this project.
                        </p>
                      )}
                    </TabsContent>
                  </Tabs>
                ) : picked ? (
                  <p className="font-medium">
                    {detail.repositories.find((repo) => repo.id === repository)
                      ?.name ?? "Linked work"}{" "}
                    #{picked.subject.number} · {picked.title}
                  </p>
                ) : null}
                {mode !== "existing" && (
                  <>
                    <Field>
                      <FieldLabel htmlFor={`${id}-title`}>Title</FieldLabel>
                      <Input
                        id={`${id}-title`}
                        required
                        maxLength={256}
                        value={item.title}
                        onChange={(event) =>
                          setItem({ ...item, title: event.target.value })
                        }
                      />
                    </Field>
                    <Field>
                      <FieldLabel htmlFor={`${id}-body`}>
                        Description
                      </FieldLabel>
                      <Textarea
                        id={`${id}-body`}
                        maxLength={16384}
                        value={item.body}
                        onChange={(event) =>
                          setItem({ ...item, body: event.target.value })
                        }
                      />
                    </Field>
                  </>
                )}
                <div className="grid gap-4 sm:grid-cols-2">
                  <Field>
                    <FieldLabel htmlFor={`${id}-status`}>Status</FieldLabel>
                    <FieldSelect
                      id={`${id}-status`}
                      label="Item status"
                      options={Object.fromEntries(
                        definition.statuses.map((status) => [status, status]),
                      )}
                      value={item.status}
                      onValueChange={(status) => setItem({ ...item, status })}
                    />
                  </Field>
                  <Field>
                    <FieldLabel htmlFor={`${id}-priority`}>Priority</FieldLabel>
                    <FieldSelect
                      id={`${id}-priority`}
                      label="Priority"
                      options={Object.fromEntries(
                        priorities.map((name, index) => [String(index), name]),
                      )}
                      value={String(item.priority)}
                      onValueChange={(priority) =>
                        setItem({ ...item, priority: Number(priority) })
                      }
                    />
                  </Field>
                </div>
                <div className="grid gap-4 sm:grid-cols-2">
                  <Field>
                    <FieldLabel htmlFor={`${id}-start`}>Start date</FieldLabel>
                    <DatePicker
                      id={`${id}-start`}
                      label="Start date"
                      value={item.start}
                      max={item.end ?? undefined}
                      disabled={commands.locked}
                      onValueChange={(start) => setItem({ ...item, start })}
                    />
                  </Field>
                  <Field>
                    <FieldLabel htmlFor={`${id}-end`}>Target date</FieldLabel>
                    <DatePicker
                      id={`${id}-end`}
                      label="Target date"
                      value={item.end}
                      min={item.start ?? undefined}
                      disabled={commands.locked}
                      onValueChange={(end) => setItem({ ...item, end })}
                    />
                  </Field>
                </div>
                {definition.fields.map((field) => (
                  <Field key={field.id}>
                    <FieldLabel htmlFor={`${id}-${field.id}`}>
                      {field.name}
                    </FieldLabel>
                    {field.definition.kind === "select" ? (
                      <FieldSelect
                        id={`${id}-${field.id}`}
                        label={field.name}
                        value={String(
                          field.definition.options.indexOf(
                            item.values[field.id] ?? "",
                          ),
                        )}
                        options={Object.fromEntries([
                          [-1, "Not set"],
                          ...field.definition.options.map((name, index) => [
                            index,
                            name,
                          ]),
                        ])}
                        onValueChange={(index) => {
                          if (field.definition.kind === "select")
                            value(
                              field.id,
                              index === "-1"
                                ? ""
                                : field.definition.options[Number(index)],
                            );
                        }}
                      />
                    ) : field.definition.kind === "date" ? (
                      <DatePicker
                        id={`${id}-${field.id}`}
                        label={field.name}
                        value={item.values[field.id] || null}
                        disabled={commands.locked}
                        onValueChange={(date) => value(field.id, date ?? "")}
                      />
                    ) : (
                      <Input
                        id={`${id}-${field.id}`}
                        type={
                          field.definition.kind === "number" ? "number" : "text"
                        }
                        step="any"
                        value={item.values[field.id] ?? ""}
                        onChange={(event) =>
                          value(field.id, event.target.value)
                        }
                      />
                    )}
                  </Field>
                ))}
              </FieldGroup>
            </fieldset>
          )}
          {saved.feedback}
          <CommandError />
          <ProjectItemReview
            target={{ organization }}
            project={detail.project.id}
            draft={item}
            onReviewed={(project, current, useSaved) => {
              setVersion(current.version);
              setDefinition(project.definition);
              if (useSaved) {
                saved.update({
                  item: current.item,
                  repository: current.repository?.id ?? "",
                  work: current.work,
                  mode: current.item.subject ? "existing" : "draft",
                  restricted: !!current.restricted,
                });
              }
            }}
          />
          <DialogFooter>
            {wasExisting && (
              <Button
                variant="destructive"
                disabled={commands.locked}
                onClick={() => void remove()}
              >
                Remove from project
              </Button>
            )}
            <Button
              type="button"
              variant="outline"
              disabled={commands.locked}
              onClick={onClose}
            >
              Cancel
            </Button>
            {!restricted && (
              <Button
                type="submit"
                disabled={
                  commands.locked ||
                  (mode === "issue" && !canWriteIssue) ||
                  (mode === "existing" ? !picked : !item.title.trim())
                }
              >
                {commands.pending
                  ? "Saving…"
                  : mode === "issue"
                    ? "Create issue"
                    : wasExisting
                      ? "Save item"
                      : "Add to project"}
              </Button>
            )}
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
