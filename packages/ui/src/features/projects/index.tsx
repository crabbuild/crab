import { Disclosure } from "@/components/ui/collapsible";
import { useEffect, useRef, useState } from "react";
import {
  FolderKanbanIcon,
  PlusIcon,
  Settings2Icon,
  RefreshCwIcon,
  ArrowLeftIcon,
  SearchIcon,
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
import { FieldSelect } from "@/components/ui/field-select";
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
import { ProjectTypeFilter } from "./issue-type";
import {
  ProjectItemPagination,
  ProjectItemSummary,
  ProjectPriorityFilter,
  ProjectStatusFilter,
} from "./project-items";
import {
  applyProjectGroup,
  ProjectGrouping,
  selectedGrouping,
  type AddProjectGroupItem,
} from "./grouping";
import type { IssueType } from "@/lib/contracts";
import { ProjectBoard } from "./board";
import { ProjectTable, ProjectRoadmap } from "./views";
import { useProjectCommands } from "./commands";
import { ProjectHistory } from "./history";
import { ProjectViewTabs } from "./view-tabs";
import { ProjectLibrary as OrganizationProjectLibrary } from "./workspace/library";
import { DraftRecovery } from "./drafts";
import { checkpointDraft, itemDraft, readDraft } from "./draft-storage";
import {
  blankItem,
  projectHref,
  type Listing,
  type Item,
  type ItemPage,
  type Project,
  type ProjectItemSelection,
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
  if (project)
    return <ProjectDetail key={project} repo={repo} url={url} id={project} />;
  const organization = repo.organization_id;
  const linked =
    organization && url.searchParams.get("collection") === "linked";
  return (
    <section className="projects-page">
      {organization && (
        <nav
          aria-label="Project collections"
          className="mb-5 flex flex-wrap gap-2"
        >
          <Button asChild variant={linked ? "ghost" : "secondary"}>
            <Link
              href={projectHref(repo)}
              aria-current={linked ? undefined : "page"}
            >
              Repository projects
            </Link>
          </Button>
          <Button asChild variant={linked ? "secondary" : "ghost"}>
            <Link
              href={`${projectHref(repo)}?collection=linked`}
              aria-current={linked ? "page" : undefined}
            >
              Linked projects
            </Link>
          </Button>
        </nav>
      )}
      {linked ? (
        <OrganizationProjectLibrary
          organization={organization}
          url={url}
          scope={{ target: { repository: repo.id }, canLink: true }}
        />
      ) : (
        <ProjectLibrary repo={repo} url={url} />
      )}
    </section>
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
    <>
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
    </>
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
  const resource = useRequest<{
    project: Project;
    can_manage: boolean;
    issue_types?: IssueType[];
  }>(endpoint(repo, `projects/${id}`));
  return (
    <Result state={resource} timingLabel="Project settings">
      {(data) => (
        <ProjectWorkspace
          repo={repo}
          url={url}
          initial={data.project}
          canManage={data.can_manage}
          typeChoices={data.issue_types ?? []}
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
  typeChoices,
}: {
  repo: Repository;
  url: URL;
  initial: Project;
  canManage: boolean;
  typeChoices: IssueType[];
}) {
  const [project, setProject] = useState(initial);
  const [editing, setEditing] = useState<ProjectItemSelection>();
  const [settings, setSettings] = useState(false);
  const selected =
    project.definition.views.find((v) => v.id === url.searchParams.get("pv")) ??
    project.definition.views[0];
  const query = url.searchParams.get("q") ?? selected.query;
  const status = url.searchParams.get("status") ?? selected.status ?? "";
  const priority =
    url.searchParams.get("priority") ?? selected.priority?.toString() ?? "";
  const groupBy = selectedGrouping(
    selected.layout,
    project.definition.fields,
    !!repo.organization_id,
    url.searchParams.get("group_by") ?? selected.group_by,
  );
  const issueType = repo.organization_id
    ? (url.searchParams.get("issue_type") ?? selected.issue_type_filter ?? "")
    : "";
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
      priority: priority || undefined,
      hide_closed: String(hidden),
      issue_type: issueType || undefined,
      group_by: groupBy,
      after,
      type_cursor: after
        ? (url.searchParams.get("type_cursor") ?? undefined)
        : undefined,
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
    next.searchParams.delete("type_cursor");
    navigate(next.pathname + next.search);
  }
  const filtered = !!(query || status || priority || hidden || issueType);
  function resetFilters() {
    setSearch("");
    filters({
      q: "",
      status: "",
      priority: "",
      hide_closed: "false",
      issue_type: "",
    });
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
  const activeView =
    url.searchParams.get("pv") === "history" ? "history" : selected.id;
  const bulkTarget =
    manager && !repo.archived && !project.archived ? { repo } : undefined;
  const editable =
    manager && !repo.archived && !project.archived && !commands.locked;
  const onIssueTypeChange =
    repo.archived || project.archived
      ? undefined
      : () => (after ? filters({}) : resource.retry());
  async function changeItem(row: Row, edit: (item: Item) => Item) {
    if (!commands.account) return;
    const scope = { target: { repo }, project: project.id, item: row.item.id };
    const previous = readDraft(commands.account, scope, "item");
    if (previous?.kind === "item") {
      checkpointDraft(commands.account, scope, {
        ...previous,
        item: edit(previous.item),
      });
      setEditing(row);
      return;
    }
    const item = edit(row.item);
    const checkpoint = checkpointDraft(commands.account, scope, {
      ...itemDraft(project, row, repo.id),
      item,
    });
    await commands.submit(
      repo,
      {
        kind: "item",
        project: project.id,
        version: row.version,
        item,
      },
      checkpoint,
    );
  }
  const addGroupItem: AddProjectGroupItem = (group, key) => {
    const draft = blankItem(status || project.definition.statuses[0]);
    setEditing({ initialItem: applyProjectGroup(draft, group, key) });
  };
  const addGroupIssue = (typeId: string | null) =>
    setEditing({
      initialItem: blankItem(status || project.definition.statuses[0]),
      initialIssueType: typeId,
    });
  const createInGroup = editable && groupBy !== undefined;
  const showEmptyView = selected.layout === "board" || createInGroup;
  const newItem =
    typeof editing === "object" && "initialItem" in editing
      ? editing
      : undefined;
  const initialItem = newItem?.initialItem;
  const editingRow =
    typeof editing === "object" && "item" in editing ? editing : undefined;
  const draftItem =
    typeof editing === "object" && "draft" in editing
      ? editing.draft
      : undefined;
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
      <DraftRecovery
        scope={{ target: { repo }, project: project.id }}
        disabled={!manager || repo.archived || project.archived}
        onResume={({ kind, item }) => {
          if (kind === "settings") setSettings(true);
          if (kind === "item") setEditing({ draft: item });
        }}
      />
      <Tabs
        value={activeView}
        onValueChange={(pv) =>
          filters({
            pv,
            q: undefined,
            status: undefined,
            priority: undefined,
            hide_closed: undefined,
            issue_type: undefined,
            group_by: undefined,
          })
        }
      >
        <ProjectViewTabs
          project={project}
          active={activeView}
          target={bulkTarget}
          editable={editable}
          preserveSelection={() => {
            if (
              activeView !== "history" &&
              url.searchParams.get("pv") !== activeView
            )
              filters({ pv: activeView });
          }}
        />
        <TabsContent value="history">
          <ProjectHistory
            target={{ repo }}
            project={project.id}
            definition={project.definition}
          />
        </TabsContent>
        <TabsContent value={selected.id}>
          <form
            aria-label="Project item filters"
            className="project-filter-bar"
            onSubmit={(e) => {
              e.preventDefault();
              filters({ q: search });
            }}
          >
            <InputGroup>
              <InputGroupAddon>
                <SearchIcon />
              </InputGroupAddon>
              <InputGroupInput
                aria-label="Filter project items"
                value={search}
                maxLength={256}
                placeholder="Filter by title…"
                onChange={(e) => setSearch(e.target.value)}
              />
            </InputGroup>
            <ProjectStatusFilter
              statuses={project.definition.statuses}
              value={status}
              onChange={(value) => filters({ status: value })}
            />
            <FieldSelect
              label="Filter work state"
              value={String(hidden)}
              onValueChange={(value) => filters({ hide_closed: value })}
              options={{ false: "All work", true: "Open work" }}
            />
            <ProjectPriorityFilter
              value={priority}
              onChange={(value) => filters({ priority: value })}
            />
            {repo.organization_id && (
              <ProjectTypeFilter
                types={typeChoices}
                value={issueType}
                onChange={(value) => filters({ issue_type: value })}
              />
            )}
            <ProjectGrouping
              layout={selected.layout}
              fields={project.definition.fields}
              issueTypes={!!repo.organization_id}
              groupBy={groupBy}
              onChange={(value) => filters({ group_by: value })}
            />
            <Button variant="outline" type="submit">
              Apply
            </Button>
            {(filtered || search) && (
              <Button variant="ghost" type="button" onClick={resetFilters}>
                Reset filters
              </Button>
            )}
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
                <ProjectItemSummary
                  page={page}
                  showNoMatches={filtered && showEmptyView}
                />
                {page.items.length || showEmptyView ? (
                  selected.layout === "board" ? (
                    <ProjectBoard
                      repo={repo}
                      page={page}
                      viewId={selected.id}
                      editable={editable && can_manage}
                      bulkTarget={bulkTarget}
                      onEdit={setEditing}
                      onAddGroupItem={addGroupItem}
                      onAddGroupIssue={addGroupIssue}
                      groupBy={groupBy}
                      onChangeItem={changeItem}
                      showIssueType={selected.show_issue_type}
                      onIssueTypeChange={onIssueTypeChange}
                    />
                  ) : selected.layout === "roadmap" ? (
                    <ProjectRoadmap
                      repo={repo}
                      groupBy={groupBy}
                      page={page}
                      editable={editable && can_manage}
                      onEdit={setEditing}
                      onAddGroupItem={addGroupItem}
                      onAddGroupIssue={addGroupIssue}
                      showIssueType={selected.show_issue_type}
                      onIssueTypeChange={onIssueTypeChange}
                    />
                  ) : (
                    <ProjectTable
                      repo={repo}
                      page={page}
                      editable={editable && can_manage}
                      bulkTarget={bulkTarget}
                      onEdit={setEditing}
                      onAddGroupItem={addGroupItem}
                      onAddGroupIssue={addGroupIssue}
                      groupBy={groupBy}
                      onChangeItem={changeItem}
                      showIssueType={selected.show_issue_type}
                      onIssueTypeChange={onIssueTypeChange}
                    />
                  )
                ) : (
                  <Empty>
                    <EmptyHeader>
                      <EmptyMedia variant="icon">
                        <FolderKanbanIcon />
                      </EmptyMedia>
                      <EmptyTitle>
                        {page.total
                          ? "No items on this page"
                          : filtered
                            ? "No work matches this view"
                            : "Your project starts here"}
                      </EmptyTitle>
                      <EmptyDescription>
                        {page.total
                          ? "Return to the first page to see matching work."
                          : filtered
                            ? "Change the filters to see more work."
                            : "Add an issue or pull request, create an issue, or capture a draft note."}
                      </EmptyDescription>
                    </EmptyHeader>
                    {editable && !filtered && !page.total && (
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
              </>
            )}
          </Result>
          {!resource.loading && (
            <ProjectItemPagination
              url={url}
              page={resource.data?.page}
              revisionKey="revision"
            />
          )}
          {project.definition.readme && (
            <Disclosure
              className="project-readme"
              summary={<>Project overview</>}
            >
              <DiscussionMarkdown>
                {project.definition.readme}
              </DiscussionMarkdown>
            </Disclosure>
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
          key={draftItem ?? editingRow?.item.id ?? initialItem?.id ?? "new"}
          repo={repo}
          project={project}
          row={editingRow}
          draftItem={draftItem}
          initialItem={initialItem}
          initialIssueType={newItem?.initialIssueType}
          typeChoices={resource.data?.page.issue_types ?? typeChoices}
          onClose={() => setEditing(undefined)}
        />
      )}
      {settings && (
        <ProjectSettings
          target={{ repo }}
          project={project}
          issueTypes={!!repo.organization_id}
          typeChoices={typeChoices}
          onClose={() => setSettings(false)}
        />
      )}
    </section>
  );
}
