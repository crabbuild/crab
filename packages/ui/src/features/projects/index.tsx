import { useEffect, useRef, useState } from "react";
import {
  FolderKanbanIcon,
  PlusIcon,
  Settings2Icon,
  RefreshCwIcon,
  ArrowLeftIcon,
  ListIcon,
  Columns3Icon,
  CalendarRangeIcon,
} from "lucide-react";
import { endpoint, navigate, useRequest, type Repository } from "@/lib/api";
import { Result } from "@/components/request/result";
import { Link } from "@/components/navigation/link";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from "@/components/ui/input-group";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
import {
  Empty,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
  EmptyDescription,
} from "@/components/ui/empty";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { CreateProject } from "./create";
import { ItemEditor } from "./item";
import { ProjectSettings } from "./settings";
import { ProjectBoard, ProjectTable, ProjectRoadmap } from "./views";
import { useProjectCommands } from "./commands";
import {
  projectHref,
  type Listing,
  type ItemPage,
  type Project,
  type Row,
} from "./model";
export default function Projects({
  repo,
  url,
  project,
}: {
  repo: Repository;
  url: URL;
  project?: string;
}) {
  return project ? (
    <ProjectDetail key={project} repo={repo} url={url} id={project} />
  ) : (
    <ProjectLibrary repo={repo} url={url} />
  );
}
function ProjectLibrary({ repo, url }: { repo: Repository; url: URL }) {
  const [after, setAfter] = useState<string>();
  const archived = url.searchParams.get("archived") === "true";
  const resource = useRequest<Listing>(
    endpoint(repo, "projects", { archived: String(archived), after }),
  );
  const commands = useProjectCommands();
  const seen = useRef(commands.generation);
  useEffect(() => {
    if (seen.current !== commands.generation) {
      seen.current = commands.generation;
      setAfter(undefined);
      resource.retry();
    }
  }, [commands.generation, resource.retry]);
  return (
    <section className="projects-page">
      <header className="project-page-header">
        <div>
          <h1>Projects</h1>
          <p>One place to shape ideas, plan work, and track delivery.</p>
        </div>
        {!repo.archived && resource.data?.can_manage && (
          <CreateProject repo={repo} />
        )}
      </header>
      <div className="project-library-toolbar">
        <Tabs
          value={archived ? "archived" : "active"}
          onValueChange={(value) => {
            setAfter(undefined);
            navigate(
              `${projectHref(repo)}${value === "archived" ? "?archived=true" : ""}`,
            );
          }}
        >
          <TabsList variant="line">
            <TabsTrigger value="active">Active projects</TabsTrigger>
            <TabsTrigger value="archived">Archived</TabsTrigger>
          </TabsList>
          <TabsContent
            value={archived ? "archived" : "active"}
            className="sr-only"
          >
            {archived ? "Archived projects" : "Active projects"}
          </TabsContent>
        </Tabs>
        <Button
          size="icon-sm"
          variant="ghost"
          aria-label="Refresh projects"
          onClick={resource.retry}
        >
          <RefreshCwIcon />
        </Button>
      </div>
      <Result state={resource} timingLabel="Projects">
        {(data) =>
          data.items.length ? (
            <>
              <div className="project-library-list">
                {data.items.map((p) => (
                  <article key={p.id}>
                    <FolderKanbanIcon aria-hidden="true" />
                    <div>
                      <h2>
                        <Link href={projectHref(repo, p.id)}>
                          {p.definition.name}
                        </Link>
                        {p.definition.is_template && (
                          <Badge variant="secondary">Template</Badge>
                        )}
                      </h2>
                      <p>{p.definition.description || "No description yet."}</p>
                    </div>
                    <time dateTime={new Date(p.updated_at).toISOString()}>
                      Updated {new Date(p.updated_at).toLocaleDateString()}
                    </time>
                  </article>
                ))}
              </div>
              {after && (
                <Button variant="outline" onClick={() => setAfter(undefined)}>
                  First page
                </Button>
              )}
              {data.next && (
                <Button variant="outline" onClick={() => setAfter(data.next!)}>
                  More projects
                </Button>
              )}
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
                    : "Make space for your next project"}
                </EmptyTitle>
                <EmptyDescription>
                  {archived
                    ? "Archived projects remain available here."
                    : "Start with a template, bring in issues and PRs, and find a workflow that fits your team."}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          )
        }
      </Result>
    </section>
  );
}
function ProjectDetail({
  repo,
  url,
  id,
}: {
  repo: Repository;
  url: URL;
  id: string;
}) {
  const resource = useRequest<{ project: Project; can_manage: boolean }>(
    endpoint(repo, `projects/${id}`),
  );
  return (
    <Result state={resource} timingLabel="Project settings">
      {(data) => (
        <ProjectWorkspace
          repo={repo}
          url={url}
          initial={data.project}
          canManage={data.can_manage}
        />
      )}
    </Result>
  );
}
function ProjectWorkspace({
  repo,
  url,
  initial,
  canManage,
}: {
  repo: Repository;
  url: URL;
  initial: Project;
  canManage: boolean;
}) {
  const [project, setProject] = useState(initial);
  const [editing, setEditing] = useState<Row | "new">();
  const [settings, setSettings] = useState(false);
  const selected =
    project.definition.views.find((v) => v.id === url.searchParams.get("pv")) ??
    project.definition.views[0];
  const query = url.searchParams.get("q") ?? selected.query;
  const status = url.searchParams.get("status") ?? selected.status ?? "";
  const hidden = url.searchParams.has("hide_closed")
    ? url.searchParams.get("hide_closed") === "true"
    : selected.hide_closed;
  const [search, setSearch] = useState(query);
  useEffect(() => setSearch(query), [query]);
  const after = url.searchParams.get("after") ?? undefined;
  const resource = useRequest<{ page: ItemPage; can_manage: boolean }>(
    endpoint(repo, `projects/${project.id}/items`, {
      q: query,
      status: status || undefined,
      hide_closed: String(hidden),
      after,
      revision: after
        ? (url.searchParams.get("revision") ?? undefined)
        : undefined,
    }),
  );
  const commands = useProjectCommands();
  const seen = useRef(commands.generation);
  function filters(values: Record<string, string | undefined>) {
    const next = new URL(url);
    for (const [key, value] of Object.entries(values)) {
      if (value === undefined) next.searchParams.delete(key);
      else next.searchParams.set(key, value);
    }
    next.searchParams.delete("after");
    next.searchParams.delete("revision");
    navigate(next.pathname + next.search);
  }
  useEffect(() => {
    if (seen.current !== commands.generation) {
      seen.current = commands.generation;
      if (after) filters({});
      else resource.retry();
    }
  }, [commands.generation, resource.retry]);
  useEffect(() => {
    if (resource.data) setProject(resource.data.page.project);
  }, [resource.data]);
  const manager = resource.data?.can_manage ?? canManage;
  const editable =
    manager && !repo.archived && !project.archived && !commands.locked;
  async function changeStatus(row: Row, status: string) {
    await commands.submit(repo, {
      kind: "item",
      project: project.id,
      version: project.version,
      item: { ...row.item, status },
    });
  }
  const Icon =
    selected.layout === "table"
      ? ListIcon
      : selected.layout === "board"
        ? Columns3Icon
        : CalendarRangeIcon;
  return (
    <section className="projects-page project-workspace">
      <Link href={projectHref(repo)} className="project-back">
        <ArrowLeftIcon />
        Projects
      </Link>
      <header className="project-page-header">
        <div>
          <h1>
            <FolderKanbanIcon aria-hidden="true" />
            {project.definition.name}
            {project.archived && <Badge variant="outline">Archived</Badge>}
          </h1>
          <p>{project.definition.description}</p>
        </div>
        <div className="flex flex-wrap gap-2">
          {manager && !repo.archived && (
            <Button
              variant="outline"
              onClick={() => setSettings(true)}
              disabled={commands.locked}
            >
              <Settings2Icon data-icon="inline-start" />
              Settings
            </Button>
          )}
          {editable && (
            <Button onClick={() => setEditing("new")}>
              <PlusIcon data-icon="inline-start" />
              Add item
            </Button>
          )}
        </div>
      </header>
      <Tabs
        value={selected.id}
        onValueChange={(pv) =>
          filters({
            pv,
            q: undefined,
            status: undefined,
            hide_closed: undefined,
          })
        }
      >
        <TabsList variant="line" className="project-view-tabs">
          {project.definition.views.map((view) => {
            const ViewIcon =
              view.layout === "board"
                ? Columns3Icon
                : view.layout === "roadmap"
                  ? CalendarRangeIcon
                  : ListIcon;
            return (
              <TabsTrigger key={view.id} value={view.id}>
                <ViewIcon />
                {view.name}
              </TabsTrigger>
            );
          })}
        </TabsList>
        <TabsContent value={selected.id}>
          <form
            className="project-filter-bar"
            onSubmit={(e) => {
              e.preventDefault();
              filters({ q: search });
            }}
          >
            <InputGroup>
              <InputGroupAddon>
                <Icon />
              </InputGroupAddon>
              <InputGroupInput
                aria-label="Filter project items"
                value={search}
                maxLength={256}
                placeholder="Filter by title…"
                onChange={(e) => setSearch(e.target.value)}
              />
            </InputGroup>
            <NativeSelect
              aria-label="Filter project status"
              value={status}
              onChange={(e) => filters({ status: e.target.value })}
            >
              <NativeSelectOption value="">All statuses</NativeSelectOption>
              {project.definition.statuses.map((s) => (
                <NativeSelectOption key={s}>{s}</NativeSelectOption>
              ))}
            </NativeSelect>
            <NativeSelect
              aria-label="Filter work state"
              value={String(hidden)}
              onChange={(e) => filters({ hide_closed: e.target.value })}
            >
              <NativeSelectOption value="false">All work</NativeSelectOption>
              <NativeSelectOption value="true">Open work</NativeSelectOption>
            </NativeSelect>
            <Button variant="outline" type="submit">
              Apply
            </Button>
            <Button
              variant="ghost"
              size="icon"
              type="button"
              aria-label="Refresh project"
              onClick={() => {
                if (after) filters({});
                else resource.retry();
              }}
            >
              <RefreshCwIcon />
            </Button>
          </form>
          <Result state={resource} timingLabel="Project items">
            {({ page, can_manage }) => (
              <>
                <div className="project-item-summary">
                  <span>
                    {page.total} matching {page.total === 1 ? "item" : "items"}
                  </span>
                  {page.total > page.items.length && (
                    <span>Showing {page.items.length} on this page</span>
                  )}
                </div>
                {page.items.length ? (
                  selected.layout === "board" ? (
                    <ProjectBoard
                      repo={repo}
                      page={page}
                      editable={editable && can_manage}
                      onEdit={setEditing}
                      onStatus={changeStatus}
                    />
                  ) : selected.layout === "roadmap" ? (
                    <ProjectRoadmap
                      repo={repo}
                      page={page}
                      editable={editable && can_manage}
                      onEdit={setEditing}
                    />
                  ) : (
                    <ProjectTable
                      repo={repo}
                      page={page}
                      editable={editable && can_manage}
                      onEdit={setEditing}
                      onStatus={changeStatus}
                    />
                  )
                ) : (
                  <Empty>
                    <EmptyHeader>
                      <EmptyMedia variant="icon">
                        <FolderKanbanIcon />
                      </EmptyMedia>
                      <EmptyTitle>
                        {query || status || hidden
                          ? "No work matches this view"
                          : "Your project starts here"}
                      </EmptyTitle>
                      <EmptyDescription>
                        {query || status || hidden
                          ? "Change the filters to see more work."
                          : "Add an issue or pull request, create an issue, or capture a draft note."}
                      </EmptyDescription>
                    </EmptyHeader>
                    {editable && (
                      <Button
                        variant="outline"
                        onClick={() => setEditing("new")}
                      >
                        <PlusIcon data-icon="inline-start" />
                        Add your first item
                      </Button>
                    )}
                  </Empty>
                )}
                {(page.next || after) && (
                  <nav
                    className="project-pagination"
                    aria-label="Project items"
                  >
                    {after && (
                      <Button variant="outline" onClick={() => filters({})}>
                        First page
                      </Button>
                    )}
                    {page.next && (
                      <Button
                        variant="outline"
                        onClick={() => {
                          const next = new URL(url);
                          next.searchParams.set("after", page.next!);
                          next.searchParams.set(
                            "revision",
                            String(page.revision),
                          );
                          navigate(next.pathname + next.search);
                        }}
                      >
                        Next 50 items
                      </Button>
                    )}
                  </nav>
                )}
              </>
            )}
          </Result>
          {project.definition.readme && (
            <details className="project-readme">
              <summary>Project overview</summary>
              <DiscussionMarkdown>
                {project.definition.readme}
              </DiscussionMarkdown>
            </details>
          )}
        </TabsContent>
        {project.definition.views
          .filter((view) => view.id !== selected.id)
          .map((view) => (
            <TabsContent key={view.id} value={view.id} />
          ))}
      </Tabs>
      {editing && (
        <ItemEditor
          repo={repo}
          project={project}
          row={editing === "new" ? undefined : editing}
          onClose={() => setEditing(undefined)}
        />
      )}
      {settings && (
        <ProjectSettings
          repo={repo}
          project={project}
          onClose={() => setSettings(false)}
        />
      )}
    </section>
  );
}
