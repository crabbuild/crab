import { ProjectDraftReview } from "./review";
import { useId, useState } from "react";
import { CircleDotIcon, GitPullRequestIcon } from "lucide-react";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { Result } from "@/components/request/result";
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
import { DatePicker } from "@/components/ui/date-picker";
import { Textarea } from "@/components/ui/textarea";
import { FieldSelect } from "@/components/ui/field-select";
import { Link } from "@/components/navigation/link";
import { useProjectCommands, CommandError } from "./commands";
import {
  blankItem,
  priorities,
  workHref,
  type Project,
  type Row,
  type Work,
} from "./model";
export function WorkPicker({
  repo,
  onSelect,
  kind,
}: {
  repo: Repository;
  onSelect: (work: Work) => void;
  kind?: "issue";
}) {
  const [query, setQuery] = useState("");
  const id = useId();
  const resource = useRequest<{ items: Work[] }>(
    endpoint(repo, "projects/candidates", { q: query, kind }),
  );
  return (
    <FieldGroup>
      <Field>
        <FieldLabel htmlFor={id}>
          {kind ? "Find an issue" : "Find an issue or pull request"}
        </FieldLabel>
        <Input
          id={id}
          placeholder="Search title or #number…"
          maxLength={256}
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
      </Field>
      <Result state={resource}>
        {(data) => (
          <div className="project-work-choices" aria-label="Matching work">
            {data.items.map((work) => (
              <Button
                className="project-work-choice"
                type="button"
                variant="ghost"
                key={`${work.subject.kind}:${work.subject.number}`}
                onClick={() => onSelect(work)}
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
            {!data.items.length && (
              <p className="muted p-3">
                No matching work. Try a different title or number.
              </p>
            )}
          </div>
        )}
      </Result>
    </FieldGroup>
  );
}
export function ItemEditor({
  repo,
  project,
  row,
  onClose,
}: {
  repo: Repository;
  project: Project;
  row?: Row;
  onClose: () => void;
}) {
  const [item, setItem] = useState(
    () => row?.item ?? blankItem(project.definition.statuses[0]),
  );
  const [mode, setMode] = useState(
    row ? (row.item.subject ? "existing" : "draft") : "issue",
  );
  const [parent, setParent] = useState("");
  const [picked, setPicked] = useState<Work | null>(row?.work ?? null);
  const commands = useProjectCommands();
  const id = useId();
  const [version, setVersion] = useState(project.version);
  const [definition, setDefinition] = useState(project.definition);
  const existing = !!row?.item.subject;
  async function save(convert = false) {
    const value = {
      ...item,
      title: item.subject ? "" : item.title.trim(),
      body: item.subject ? "" : item.body,
    };
    const result = await commands.submit(
      repo,
      (mode === "issue" && !existing) || convert
        ? {
            kind: "create_issue",
            project: project.id,
            version,
            item: { ...value, subject: null },
            parent: parent ? Number(parent) : null,
          }
        : { kind: "item", project: project.id, version, item: value },
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
      <DialogContent
        className="project-item-dialog"
        onEscapeKeyDown={(e) => {
          if (commands.locked) e.preventDefault();
        }}
        onInteractOutside={(e) => {
          if (commands.locked) e.preventDefault();
        }}
      >
        <DialogHeader>
          <DialogTitle>
            {row ? "Project item" : "Add work to project"}
          </DialogTitle>
          <DialogDescription>
            {project.definition.name} · Changes to planning fields apply to this
            project.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex min-h-0 flex-col gap-5"
          onSubmit={(e) => {
            e.preventDefault();
            void save();
          }}
        >
          <fieldset disabled={commands.locked} className="project-form-scroll">
            <FieldGroup>
              {!row && (
                <Field>
                  <FieldLabel htmlFor={`${id}-mode`}>Add</FieldLabel>
                  <FieldSelect
                    label="Add"
                    id={`${id}-mode`}
                    value={mode}
                    onValueChange={(value) => {
                      setMode(value);
                      setItem({
                        ...blankItem(project.definition.statuses[0]),
                        id: item.id,
                      });
                      setPicked(null);
                    }}
                    options={{
                      issue: "Create an issue",
                      existing: "Existing issue or pull request",
                      draft: "Draft note",
                    }}
                  />
                </Field>
              )}
              {mode === "existing" && !existing && !picked && (
                <WorkPicker
                  repo={repo}
                  onSelect={(work) => {
                    setPicked(work);
                    setItem({
                      ...item,
                      subject: work.subject,
                      title: "",
                      body: "",
                    });
                  }}
                />
              )}
              {picked && (
                <Field>
                  <FieldLabel>Linked work</FieldLabel>
                  <Link href={workHref(repo, picked.subject)}>
                    #{picked.subject.number} {picked.title}
                  </Link>
                  <FieldDescription>
                    Title, assignees, and open/closed state are managed on the{" "}
                    {picked.subject.kind === "issue" ? "issue" : "pull request"}
                    .
                  </FieldDescription>
                  {!existing && (
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      onClick={() => {
                        setPicked(null);
                        setItem({ ...item, subject: null });
                      }}
                    >
                      Choose different work
                    </Button>
                  )}
                </Field>
              )}
              {!item.subject && (
                <>
                  <Field>
                    <FieldLabel htmlFor={`${id}-title`}>Title</FieldLabel>
                    <Input
                      id={`${id}-title`}
                      value={item.title}
                      maxLength={256}
                      required
                      onChange={(e) =>
                        setItem({ ...item, title: e.target.value })
                      }
                    />
                  </Field>
                  <Field>
                    <FieldLabel htmlFor={`${id}-body`}>Description</FieldLabel>
                    <Textarea
                      id={`${id}-body`}
                      rows={3}
                      value={item.body}
                      maxLength={16384}
                      onChange={(e) =>
                        setItem({ ...item, body: e.target.value })
                      }
                    />
                  </Field>
                  {mode === "issue" && (
                    <Field>
                      <FieldLabel htmlFor={`${id}-parent`}>
                        Parent issue number (optional)
                      </FieldLabel>
                      <Input
                        id={`${id}-parent`}
                        type="number"
                        min={1}
                        step={1}
                        value={parent}
                        onChange={(e) => setParent(e.target.value)}
                      />
                      <FieldDescription>
                        Create this as a sub-issue. The issue and project item
                        are saved together.
                      </FieldDescription>
                    </Field>
                  )}
                </>
              )}
              <div className="project-field-grid">
                <Field>
                  <FieldLabel htmlFor={`${id}-status`}>Status</FieldLabel>
                  <FieldSelect
                    label="Status"
                    id={`${id}-status`}
                    value={item.status}
                    onValueChange={(status) => setItem({ ...item, status })}
                    options={Object.fromEntries(
                      definition.statuses.map((status) => [status, status]),
                    )}
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor={`${id}-priority`}>Priority</FieldLabel>
                  <FieldSelect
                    label="Priority"
                    id={`${id}-priority`}
                    value={String(item.priority)}
                    onValueChange={(priority) =>
                      setItem({ ...item, priority: Number(priority) })
                    }
                    options={Object.fromEntries(
                      priorities.map((label, index) => [String(index), label]),
                    )}
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor={`${id}-start`}>Start date</FieldLabel>
                  <DatePicker
                    id={`${id}-start`}
                    label="Start date"
                    max={item.end ?? undefined}
                    value={item.start}
                    disabled={commands.locked}
                    onValueChange={(start) => setItem({ ...item, start })}
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor={`${id}-end`}>Target date</FieldLabel>
                  <DatePicker
                    id={`${id}-end`}
                    label="Target date"
                    min={item.start ?? undefined}
                    value={item.end}
                    disabled={commands.locked}
                    onValueChange={(end) => setItem({ ...item, end })}
                  />
                </Field>
                {definition.fields.map((field) => (
                  <Field key={field.id}>
                    <FieldLabel htmlFor={`${id}-${field.id}`}>
                      {field.name}
                    </FieldLabel>
                    {field.definition.kind === "select" ? (
                      <FieldSelect
                        label={field.name}
                        id={`${id}-${field.id}`}
                        value={String(
                          field.definition.options.indexOf(
                            item.values[field.id] ?? "",
                          ),
                        )}
                        onValueChange={(index) => {
                          const values = { ...item.values };
                          if (field.definition.kind !== "select") return;
                          if (index !== "-1")
                            values[field.id] =
                              field.definition.options[Number(index)];
                          else delete values[field.id];
                          setItem({ ...item, values });
                        }}
                        options={{
                          "-1": "Not set",
                          ...Object.fromEntries(
                            field.definition.options.map((option, index) => [
                              String(index),
                              option,
                            ]),
                          ),
                        }}
                      />
                    ) : field.definition.kind === "date" ? (
                      <DatePicker
                        id={`${id}-${field.id}`}
                        label={field.name}
                        value={item.values[field.id] || null}
                        disabled={commands.locked}
                        onValueChange={(date) => {
                          const values = { ...item.values };
                          if (date) values[field.id] = date;
                          else delete values[field.id];
                          setItem({ ...item, values });
                        }}
                      />
                    ) : (
                      <Input
                        id={`${id}-${field.id}`}
                        type={
                          field.definition.kind === "number" ? "number" : "text"
                        }
                        step={
                          field.definition.kind === "number" ? "any" : undefined
                        }
                        maxLength={1000}
                        value={item.values[field.id] ?? ""}
                        onChange={(e) => {
                          const values = { ...item.values };
                          if (e.target.value) values[field.id] = e.target.value;
                          else delete values[field.id];
                          setItem({ ...item, values });
                        }}
                      />
                    )}
                  </Field>
                ))}
              </div>
            </FieldGroup>
          </fieldset>
          <CommandError />
          <ProjectDraftReview
            target={{ repo }}
            project={project.id}
            onReviewed={(latest) => {
              setVersion(latest.version);
              setDefinition(latest.definition);
            }}
          />
          <DialogFooter>
            {row && !row.item.subject && (
              <Button
                type="button"
                variant="outline"
                disabled={commands.locked || !item.title.trim()}
                onClick={() => void save(true)}
              >
                Convert to issue
              </Button>
            )}
            {row && (
              <Button
                type="button"
                variant="destructive"
                disabled={commands.locked}
                onClick={async () => {
                  const result = await commands.submit(repo, {
                    kind: "remove",
                    project: project.id,
                    version,
                    item: item.id,
                  });
                  if (result?.outcome === "applied") onClose();
                }}
              >
                Remove from project
              </Button>
            )}
            <Button
              disabled={
                commands.locked ||
                (!item.subject && !item.title.trim()) ||
                (mode === "existing" && !item.subject)
              }
              type="submit"
            >
              {commands.locked
                ? "Saving…"
                : row
                  ? "Save item"
                  : "Add to project"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
