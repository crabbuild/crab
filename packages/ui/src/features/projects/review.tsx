import { useState } from "react";
import { useRequest } from "@/lib/api";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { useProjectCommands } from "./commands";
import {
  projectEndpoint,
  type Item,
  type ItemPage,
  type Project,
  type ProjectTarget,
  type Row,
} from "./model";
import {
  Table,
  TableHeader,
  TableHead,
  TableBody,
  TableRow,
  TableCell,
} from "@/components/ui/table";
export function ProjectDraftReview({
  target,
  project,
  onReviewed,
}: {
  target: ProjectTarget;
  project: string;
  onReviewed: (latest: Project) => void;
}) {
  const [open, setOpen] = useState(false);
  const commands = useProjectCommands();
  const resource = useRequest<{ project: Project; can_manage: boolean }>(
    open ? projectEndpoint(target, project) : null,
  );
  if (!commands.error || commands.locked) return null;
  return (
    <section className="flex flex-col gap-3" aria-label="Review saved project">
      {!open ? (
        <Button type="button" variant="outline" onClick={() => setOpen(true)}>
          Review saved project
        </Button>
      ) : (
        <Result state={resource}>
          {(data) => (
            <>
              <p>
                Your draft is still in this form. The saved project is{" "}
                <strong>{data.project.definition.name}</strong>, version{" "}
                {data.project.version}
                {data.project.archived ? ", archived" : ""}.
              </p>
              <p className="muted">
                Workflow: {data.project.definition.statuses.join(" · ")}
              </p>
              {data.can_manage &&
                !("repo" in target && target.repo.archived) && (
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => {
                      onReviewed(data.project);
                      commands.clearError();
                      setOpen(false);
                    }}
                  >
                    Keep my draft against this version
                  </Button>
                )}
            </>
          )}
        </Result>
      )}
    </section>
  );
}

export function ProjectItemReview({
  target,
  project,
  draft,
  onReviewed,
}: {
  target: ProjectTarget;
  project: string;
  draft: Item;
  onReviewed: (project: Project, row: Row, useSaved: boolean) => void;
}) {
  const [open, setOpen] = useState(false);
  const commands = useProjectCommands();
  const resource = useRequest<{ page: ItemPage; can_manage: boolean }>(
    open
      ? projectEndpoint(target, `${project}/items`, { item: draft.id })
      : null,
  );
  if (!commands.error || commands.locked) return null;
  const display = (value: unknown) =>
    value === null || value === undefined || value === ""
      ? "Not set"
      : typeof value === "string"
        ? value
        : JSON.stringify(value);
  return (
    <section
      className="flex min-w-0 flex-col gap-3"
      aria-label="Review saved item"
    >
      {!open ? (
        <Button type="button" variant="outline" onClick={() => setOpen(true)}>
          Review saved item
        </Button>
      ) : (
        <Result state={resource}>
          {({ page, can_manage }) => {
            const saved = page.items[0];
            if (!saved)
              return (
                <p role="alert">
                  This item was removed. Your draft remains in this form; copy
                  any text you want to retain before closing it.
                </p>
              );
            const fields = [
              "title",
              "body",
              "status",
              "priority",
              "start",
              "end",
              "values",
            ] as const;
            const changed = fields.filter(
              (field) =>
                JSON.stringify(saved.item[field]) !==
                JSON.stringify(draft[field]),
            );
            return (
              <>
                <p>
                  The saved item is revision {saved.version}. Your draft remains
                  in the form.
                </p>
                {changed.length ? (
                  <div className="max-h-64 overflow-auto rounded-md border">
                    <Table>
                      <TableHeader>
                        <TableRow>
                          <TableHead>Field</TableHead>
                          <TableHead>Saved value</TableHead>
                          <TableHead>Your draft</TableHead>
                        </TableRow>
                      </TableHeader>
                      <TableBody>
                        {changed.map((field) => (
                          <TableRow key={field}>
                            <TableCell>
                              {field === "end"
                                ? "Target date"
                                : field === "start"
                                  ? "Start date"
                                  : field === "values"
                                    ? "Custom fields"
                                    : field}
                            </TableCell>
                            <TableCell className="max-w-64 whitespace-pre-wrap break-words">
                              {display(saved.item[field])}
                            </TableCell>
                            <TableCell className="max-w-64 whitespace-pre-wrap break-words">
                              {display(draft[field])}
                            </TableCell>
                          </TableRow>
                        ))}
                      </TableBody>
                    </Table>
                  </div>
                ) : (
                  <p>No planning field values differ from your draft.</p>
                )}
                {can_manage && !page.project.archived && (
                  <div className="flex flex-wrap gap-2">
                    <Button
                      type="button"
                      variant="outline"
                      onClick={() => {
                        onReviewed(page.project, saved, false);
                        commands.clearError();
                        setOpen(false);
                      }}
                    >
                      Keep my draft against this revision
                    </Button>
                    <Button
                      type="button"
                      variant="outline"
                      onClick={() => {
                        onReviewed(page.project, saved, true);
                        commands.clearError();
                        setOpen(false);
                      }}
                    >
                      Use saved values
                    </Button>
                  </div>
                )}
              </>
            );
          }}
        </Result>
      )}
    </section>
  );
}
