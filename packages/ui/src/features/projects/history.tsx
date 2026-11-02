import { useEffect, useRef, useState } from "react";
import { ChevronDownIcon } from "lucide-react";
import { useRequest } from "@/lib/api";
import { Result } from "@/components/request/result";
import { RelativeTime } from "@/components/discussion/time";
import { Button } from "@/components/ui/button";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Table,
  TableHeader,
  TableHead,
  TableBody,
  TableRow,
  TableCell,
} from "@/components/ui/table";
import { Link } from "@/components/navigation/link";
import { useProjectCommands } from "./commands";
import {
  projectEndpoint,
  workHref,
  type Definition,
  type ProjectTarget,
  type Subject,
} from "./model";

interface Event {
  version: number;
  at_ms: number;
  actor: ({ id: string } | { name: string }) & {
    profile: { name: string } | null;
  };
  target: {
    kind: "project" | "properties" | "repository" | "item";
    id?: string;
    restricted?: boolean;
    source?: {
      repository: { owner: string; name: string };
      subject: Subject;
    } | null;
  };
  changes: { field: string; before: unknown; after: unknown }[];
}
export function ProjectHistory({
  target,
  project,
  definition,
}: {
  target: ProjectTarget;
  project: string;
  definition: Definition;
}) {
  const [before, setBefore] = useState<number>();
  const commands = useProjectCommands();
  const generation = useRef(commands.generation);
  const resource = useRequest<{ items: Event[]; next: number | null }>(
    projectEndpoint(target, `${project}/history`, {
      before: before?.toString(),
    }),
  );
  useEffect(() => {
    if (generation.current !== commands.generation) {
      generation.current = commands.generation;
      if (before !== undefined) setBefore(undefined);
      else resource.retry();
    }
  }, [commands.generation, before, resource.retry]);
  function label(path: string) {
    const parts = path
      .split("/")
      .slice(1)
      .map((part) => part.replaceAll("~1", "/").replaceAll("~0", "~"));
    if (parts[0] === "values")
      return (
        definition.fields.find((field) => field.id === parts[1])?.name ??
        "Custom field"
      );
    return parts
      .filter((part) => part !== "definition")
      .map(
        (part) =>
          ({
            end: "Target date",
            start: "Start date",
            target: "Target date",
            readme: "README",
            is_template: "Template",
          })[part] ?? part.replaceAll("_", " "),
      )
      .join(" · ");
  }
  const display = (value: unknown) =>
    value === null || value === undefined
      ? "Not set"
      : typeof value === "string"
        ? value || "Empty"
        : JSON.stringify(value);
  return (
    <section className="min-w-0 py-4" aria-label="Project history">
      <Result state={resource}>
        {({ items, next }) => (
          <>
            {!items.length && (
              <p className="py-8 text-sm text-muted-foreground">
                No recorded project changes in this window.
              </p>
            )}
            <ol className="divide-y">
              {items.map((event) => (
                <li key={event.version} className="py-4">
                  <Collapsible>
                    <div className="flex flex-wrap items-baseline justify-between gap-2 text-sm">
                      <p>
                        <strong>
                          {event.actor.profile?.name ??
                            ("name" in event.actor
                              ? event.actor.name
                              : `Member ${event.actor.id}`)}
                        </strong>{" "}
                        updated{" "}
                        {event.target.kind === "item"
                          ? "a project item"
                          : event.target.kind === "repository"
                            ? "a repository link"
                            : "the project"}{" "}
                        · <RelativeTime value={event.at_ms} />
                      </p>
                      <span className="text-muted-foreground">
                        Revision {event.version}
                      </span>
                    </div>
                    {event.target.source && (
                      <Link
                        className="mt-2 inline-block text-sm"
                        href={workHref(
                          event.target.source.repository,
                          event.target.source.subject,
                        )}
                      >
                        {event.target.source.repository.owner}/
                        {event.target.source.repository.name} #
                        {event.target.source.subject.number}
                      </Link>
                    )}
                    {event.target.restricted && (
                      <p className="mt-2 text-sm text-muted-foreground">
                        Repository access is required to view this source.
                      </p>
                    )}
                    <CollapsibleTrigger asChild>
                      <Button className="mt-2" variant="ghost" size="sm">
                        View {event.changes.length} field changes
                        <ChevronDownIcon data-icon="inline-end" />
                      </Button>
                    </CollapsibleTrigger>
                    <CollapsibleContent>
                      <div className="mt-2 max-h-80 overflow-auto rounded-md border">
                        <Table>
                          <TableHeader>
                            <TableRow>
                              <TableHead>Field</TableHead>
                              <TableHead>Previous value</TableHead>
                              <TableHead>New value</TableHead>
                            </TableRow>
                          </TableHeader>
                          <TableBody>
                            {event.changes.map((change) => (
                              <TableRow key={change.field}>
                                <TableCell className="capitalize">
                                  {label(change.field)}
                                </TableCell>
                                <TableCell className="max-w-80 whitespace-pre-wrap break-words">
                                  {display(change.before)}
                                </TableCell>
                                <TableCell className="max-w-80 whitespace-pre-wrap break-words">
                                  {display(change.after)}
                                </TableCell>
                              </TableRow>
                            ))}
                          </TableBody>
                        </Table>
                      </div>
                    </CollapsibleContent>
                  </Collapsible>
                </li>
              ))}
            </ol>
            <nav
              className="flex flex-wrap gap-2 pt-4"
              aria-label="Project history pages"
            >
              {before !== undefined && (
                <Button variant="outline" onClick={() => setBefore(undefined)}>
                  Latest changes
                </Button>
              )}
              {next !== null && (
                <Button variant="outline" onClick={() => setBefore(next)}>
                  Older changes
                </Button>
              )}
            </nav>
          </>
        )}
      </Result>
    </section>
  );
}
