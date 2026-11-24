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
import { DraftRecovery } from "../drafts";
import { LinkTeamProject } from "./link-team";
import {
  statuses,
  health,
  workspaceHref,
  type WorkspaceListing,
} from "./model";

export function ProjectLibrary({
  organization,
  url,
  team,
}: {
  organization: string;
  url: URL;
  team?: { id: string; canManage: boolean };
}) {
  const [creating, setCreating] = useState(false);
  const [linking, setLinking] = useState(false);
  const [after, setAfter] = useState<string>();
  const [revision, setRevision] = useState<string>();
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  const archived = url.searchParams.get("archived") === "true";
  const resource = useRequest<WorkspaceListing>(
    projectEndpoint({ organization }, "", {
      archived: String(archived),
      search: query,
      after,
      revision,
      team: team?.id,
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
          {team ? (
            <h3 className="text-lg font-semibold">Team projects</h3>
          ) : (
            <>
              <h1>Projects</h1>
              <p>Plan and track delivery across your repositories.</p>
            </>
          )}
        </div>
        {team?.canManage && (
          <Button onClick={() => setLinking(true)} disabled={commands.locked}>
            <PlusIcon data-icon="inline-start" />
            Link a project
          </Button>
        )}
        {!team && resource.data?.can_create && (
          <Button onClick={() => setCreating(true)} disabled={commands.locked}>
            <PlusIcon data-icon="inline-start" />
            New project
          </Button>
        )}
      </header>
      {!team && (
        <DraftRecovery
          scope={{ target: { organization }, project: null }}
          disabled={!resource.data?.can_create}
          onResume={() => setCreating(true)}
        />
      )}
      <div className="workspace-project-filters">
        <Tabs
          value={archived ? "archived" : "active"}
          onValueChange={(value) => {
            setAfter(undefined);
            setRevision(undefined);
            const next = new URL(url);
            if (value === "archived") next.searchParams.set("archived", "true");
            else next.searchParams.delete("archived");
            navigate(next.pathname + next.search);
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
      <Result state={resource} timingLabel="Projects">
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
                      : team
                        ? "No linked projects"
                        : "Plan your next project"}
                </EmptyTitle>
                {(query || !team) && (
                  <EmptyDescription>
                    {query
                      ? "Try another project name."
                      : "Create a project, link repositories, and bring the work together."}
                  </EmptyDescription>
                )}
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
      {team && linking && (
        <LinkTeamProject
          organization={organization}
          team={team.id}
          onClose={() => setLinking(false)}
        />
      )}
    </section>
  );
}
