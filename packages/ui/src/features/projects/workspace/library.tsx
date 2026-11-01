import { useEffect, useRef, useState } from "react";
import { FolderKanbanIcon, PlusIcon, SearchIcon } from "lucide-react";
import { navigate, useRequest } from "@/lib/api";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import {
  Table,
  TableHeader,
  TableHead,
  TableBody,
  TableCell,
  TableRow,
} from "@/components/ui/table";
import {
  Empty,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
  EmptyDescription,
} from "@/components/ui/empty";
import { useProjectCommands } from "../commands";
import { projectEndpoint } from "../model";
import { NewProject } from "./forms";
import {
  statuses,
  health,
  workspaceHref,
  type WorkspaceProject,
} from "./model";

export function ProjectLibrary({
  organization,
  url,
}: {
  organization: string;
  url: URL;
}) {
  const [creating, setCreating] = useState(false);
  const [after, setAfter] = useState<string>();
  const [revision, setRevision] = useState<string>();
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  const archived = url.searchParams.get("archived") === "true";
  const resource = useRequest<{
    items: WorkspaceProject[];
    next: string | null;
    revision: number;
    can_create: boolean;
  }>(
    projectEndpoint({ organization }, "", {
      archived: String(archived),
      search: query,
      after,
      revision,
    }),
  );
  const commands = useProjectCommands();
  const generation = useRef(commands.generation);
  useEffect(() => {
    const timer = setTimeout(() => {
      setQuery(search);
      setAfter(undefined);
      setRevision(undefined);
    }, 180);
    return () => clearTimeout(timer);
  }, [search]);
  useEffect(() => {
    if (generation.current !== commands.generation) {
      generation.current = commands.generation;
      setAfter(undefined);
      resource.retry();
    }
  }, [commands.generation, resource.retry]);
  return (
    <section className="workspace-project-library">
      <header className="workspace-project-header">
        <div>
          <h1>Projects</h1>
          <p>Plan and track delivery across your repositories.</p>
        </div>
        {resource.data?.can_create && (
          <Button onClick={() => setCreating(true)} disabled={commands.locked}>
            <PlusIcon data-icon="inline-start" />
            New project
          </Button>
        )}
      </header>
      <div className="workspace-project-filters">
        <Tabs
          value={archived ? "archived" : "active"}
          onValueChange={(value) => {
            setAfter(undefined);
            setRevision(undefined);
            navigate(
              `${workspaceHref(organization)}${value === "archived" ? "?archived=true" : ""}`,
            );
          }}
        >
          <TabsList variant="line">
            <TabsTrigger value="active">Active</TabsTrigger>
            <TabsTrigger value="archived">Archived</TabsTrigger>
          </TabsList>
          <TabsContent
            value={archived ? "archived" : "active"}
            className="sr-only"
          >
            {archived ? "Archived projects" : "Active projects"}
          </TabsContent>
        </Tabs>
        <InputGroup className="workspace-project-search">
          <InputGroupAddon>
            <SearchIcon />
          </InputGroupAddon>
          <InputGroupInput
            aria-label="Search projects"
            placeholder="Search projects…"
            maxLength={100}
            value={search}
            onChange={(event) => setSearch(event.target.value)}
          />
        </InputGroup>
      </div>
      {resource.error && after && (
        <Button
          variant="outline"
          onClick={() => {
            setAfter(undefined);
            setRevision(undefined);
          }}
        >
          Return to first page
        </Button>
      )}
      <Result state={resource}>
        {(data) =>
          data.items.length ? (
            <>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Project</TableHead>
                    <TableHead>Status</TableHead>
                    <TableHead>Health</TableHead>
                    <TableHead>Target date</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {data.items.map((project) => (
                    <TableRow key={project.id}>
                      <TableCell>
                        <Link
                          className="workspace-project-link"
                          href={workspaceHref(organization, project.id)}
                        >
                          <FolderKanbanIcon aria-hidden="true" />
                          <span>
                            <strong>{project.definition.name}</strong>
                            {project.definition.description && (
                              <small>{project.definition.description}</small>
                            )}
                          </span>
                        </Link>
                      </TableCell>
                      <TableCell>
                        <Badge variant="secondary">
                          {statuses[project.properties.status]}
                        </Badge>
                      </TableCell>
                      <TableCell>
                        <Badge
                          variant={
                            project.properties.health === "on_track"
                              ? "outline"
                              : "warning"
                          }
                        >
                          {health[project.properties.health]}
                        </Badge>
                      </TableCell>
                      <TableCell>
                        {project.properties.target ?? "No target date"}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
              <div className="workspace-pagination">
                {after && (
                  <Button
                    variant="outline"
                    onClick={() => {
                      setAfter(undefined);
                      setRevision(undefined);
                    }}
                  >
                    First page
                  </Button>
                )}
                {data.next && (
                  <Button
                    variant="outline"
                    onClick={() => {
                      setAfter(data.next!);
                      setRevision(String(data.revision));
                    }}
                  >
                    More projects
                  </Button>
                )}
              </div>
            </>
          ) : (
            <Empty>
              <EmptyHeader>
                <EmptyMedia variant="icon">
                  <FolderKanbanIcon />
                </EmptyMedia>
                <EmptyTitle>
                  {archived
                    ? "No archived projects"
                    : query
                      ? "No matching projects"
                      : "Plan your next project"}
                </EmptyTitle>
                <EmptyDescription>
                  {query
                    ? "Try another project name."
                    : "Create a project, link repositories, and bring the work together."}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          )
        }
      </Result>
      {creating && (
        <NewProject
          organization={organization}
          onClose={() => setCreating(false)}
        />
      )}
    </section>
  );
}
