import { useEffect, useRef, useState } from "react";
import {
  ArrowLeftIcon,
  FolderKanbanIcon,
  PlusIcon,
  Settings2Icon,
  UsersIcon,
} from "lucide-react";
import { navigate, useRequest } from "@/lib/api";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Tabs, TabsContent } from "@/components/ui/tabs";
import {
  Empty,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
  EmptyDescription,
} from "@/components/ui/empty";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import type { OrganizationView } from "@/lib/contracts";
import { useProjectCommands } from "../commands";
import { ProjectHistory } from "../history";
import { ProjectViewTabs } from "../view-tabs";
import { DraftRecovery } from "../drafts";
import { checkpointDraft, itemDraft, readDraft } from "../draft-storage";
import {
  blankItem,
  projectEndpoint,
  type Item,
  type ItemPage,
  type ProjectItemSelection,
  type Row,
} from "../model";
import { ProjectSettings } from "../settings";
import { ProjectTypeFilter } from "../issue-type";
import {
  applyProjectGroup,
  ProjectGrouping,
  selectedGrouping,
  type AddProjectGroupItem,
} from "../grouping";
import { ProjectBoard } from "../board";
import { ProjectTable, ProjectRoadmap } from "../views";
import { FieldSelect } from "@/components/ui/field-select";
import { ProjectProperties } from "./forms";
import { ProjectAccess } from "./access";
import { ProjectLibrary } from "./library";
import { WorkspaceItemEditor } from "./item";
import { Repositories } from "./repositories";
import {
  statuses,
  health,
  workspaceHref,
  type Detail,
  type LinkedRepository,
} from "./model";

export function OrganizationProjects({
  organization,
  project,
  url,
}: {
  organization: string;
  project?: string;
  url: URL;
}) {
  const own = useRequest<OrganizationView>(
    `/api/organizations/${organization}`,
  );
  return (
    <Result state={own}>
      {({ organization: record }) => (
        <div className="organization-project-shell">
          <aside className="organization-project-nav">
            <Link
              className="organization-project-name"
              href={`/-/organizations/${organization}`}
            >
              {record.name}
            </Link>
            <nav aria-label="Organization workspace">
              <Button asChild variant="secondary">
                <Link href={workspaceHref(organization)} aria-current="page">
                  <FolderKanbanIcon data-icon="inline-start" />
                  Projects
                </Link>
              </Button>
              <Button asChild variant="ghost">
                <Link href={`/-/organizations/${organization}?tab=people`}>
                  <UsersIcon data-icon="inline-start" />
                  People
                </Link>
              </Button>
            </nav>
          </aside>
          <section
            className="organization-project-main"
            aria-label="Project workspace"
          >
            {project ? (
              <Workspace
                key={project}
                organization={organization}
                id={project}
                url={url}
              />
            ) : (
              <ProjectLibrary organization={organization} url={url} />
            )}
          </section>
        </div>
      )}
    </Result>
  );
}
function Workspace({
  organization,
  id,
  url,
}: {
  organization: string;
  id: string;
  url: URL;
}) {
  const detail = useRequest<Detail>(projectEndpoint({ organization }, id));
  const [confirmed, setConfirmed] = useState<Detail>();
  useEffect(() => {
    if (detail.data) setConfirmed(detail.data);
  }, [detail.data]);
  const commands = useProjectCommands();
  const generation = useRef(commands.generation);
  useEffect(() => {
    if (generation.current !== commands.generation) {
      generation.current = commands.generation;
      detail.retry();
    }
  }, [commands.generation, detail.retry]);
  const current = detail.data ?? confirmed;
  if (!current || detail.error)
    return <Result state={detail}>{() => null}</Result>;
  // A confirmed mutation refreshes metadata without unmounting open editors or
  // the repository picker. New writes wait for that project's next version.
  return (
    <PlanningWorkspace
      organization={organization}
      detail={current}
      url={url}
      refreshing={detail.loading}
    />
  );
}
function PlanningWorkspace({
  organization,
  detail,
  url,
  refreshing,
}: {
  organization: string;
  detail: Detail;
  url: URL;
  refreshing: boolean;
}) {
  const { project } = detail;
  const commands = useProjectCommands();
  const generation = useRef(commands.generation);
  const [editing, setEditing] = useState<ProjectItemSelection>();
  const [settings, setSettings] = useState(false);
  const [access, setAccess] = useState(false);
  const [properties, setProperties] = useState(false);
  const overview = url.searchParams.get("view") === "overview";
  const selected =
    project.definition.views.find(
      (view) => view.id === url.searchParams.get("pv"),
    ) ?? project.definition.views[0];
  const activeView =
    url.searchParams.get("view") === "history"
      ? "history"
      : overview
        ? "overview"
        : selected.id;
  const requestedLayout = url.searchParams.get("layout");
  const layout =
    requestedLayout === "table" ||
    requestedLayout === "board" ||
    requestedLayout === "roadmap"
      ? requestedLayout
      : selected.layout;
  const groupBy = selectedGrouping(
    layout,
    project.definition.fields,
    true,
    url.searchParams.get("group_by") ?? selected.group_by,
  );
  const status = url.searchParams.get("status") ?? selected.status ?? "";
  const issueType =
    url.searchParams.get("issue_type") ?? selected.issue_type_filter ?? "";
  const repository = url.searchParams.get("repository") ?? "";
  const after = url.searchParams.get("after") ?? undefined;
  const [loadRepositories, setLoadRepositories] = useState(false);
  const repositoryChoices = useRequest<{ items: LinkedRepository[] }>(
    loadRepositories
      ? projectEndpoint({ organization }, `${project.id}/filter-repositories`)
      : null,
  );
  const resource = useRequest<{ page: ItemPage; can_manage: boolean }>(
    projectEndpoint({ organization }, `${project.id}/items`, {
      status: !overview && status ? status : undefined,
      repository: !overview && repository ? repository : undefined,
      issue_type: !overview && issueType ? issueType : undefined,
      group_by: overview ? undefined : groupBy,
      after: overview ? undefined : after,
      type_cursor:
        !overview && after
          ? (url.searchParams.get("type_cursor") ?? undefined)
          : undefined,
      version: after
        ? (url.searchParams.get("version") ?? undefined)
        : undefined,
    }),
  );
  const filterRepositories = new Map(
    [
      ...detail.repositories,
      ...(repositoryChoices.data?.items ?? []),
      ...(resource.data?.page.items.flatMap((row) =>
        row.repository ? [row.repository] : [],
      ) ?? []),
    ].map((repo) => [repo.id, `${repo.owner}/${repo.name}`]),
  );
  function filters(values: Record<string, string | undefined>) {
    const next = new URL(url);
    for (const [key, value] of Object.entries(values)) {
      if (value === undefined) next.searchParams.delete(key);
      else next.searchParams.set(key, value);
    }
    next.searchParams.delete("after");
    next.searchParams.delete("version");
    next.searchParams.delete("type_cursor");
    navigate(next.pathname + next.search);
  }
  useEffect(() => {
    if (generation.current !== commands.generation) {
      generation.current = commands.generation;
      if (after) filters({});
      else resource.retry();
    }
  }, [commands.generation, resource.retry]);
  const editable =
    detail.can_manage && !project.archived && !commands.locked && !refreshing;
  async function changeItem(row: Row, edit: (item: Item) => Item) {
    if (row.restricted) return;
    if (!commands.account) return;
    const scope = {
      target: { organization },
      project: project.id,
      item: row.item.id,
    };
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
      ...itemDraft(project, row, row.source_repository ?? ""),
      item,
    });
    await commands.submitOrganization(
      organization,
      {
        kind: "item",
        project: project.id,
        version: row.version,
        repository: row.source_repository ?? null,
        item,
      },
      checkpoint,
    );
  }
  const addGroupItem: AddProjectGroupItem = (group, key) => {
    const draft = blankItem(status || project.definition.statuses[0]);
    setEditing({ initialItem: applyProjectGroup(draft, group, key) });
  };
  const canCreateIssue = detail.repositories.some(
    (repo) => repo.can_create_issue,
  );
  const addGroupIssue = (typeId: string | null) => {
    setEditing({
      initialItem: blankItem(status || project.definition.statuses[0]),
      initialIssueType: typeId,
    });
  };
  const createInGroup =
    editable &&
    groupBy !== undefined &&
    (groupBy !== "issue_type" || canCreateIssue);
  const initialItem =
    typeof editing === "object" && "initialItem" in editing
      ? editing.initialItem
      : undefined;
  const editingRow =
    typeof editing === "object" && "item" in editing ? editing : undefined;
  const draftItem =
    typeof editing === "object" && "draft" in editing
      ? editing.draft
      : undefined;
  const initialIssueType =
    typeof editing === "object" && "initialItem" in editing
      ? editing.initialIssueType
      : undefined;
  const View =
    layout === "board"
      ? ProjectBoard
      : layout === "roadmap"
        ? ProjectRoadmap
        : ProjectTable;
  return (
    <section className="workspace-project-detail">
      <Link
        className="workspace-project-back"
        href={workspaceHref(organization)}
      >
        <ArrowLeftIcon aria-hidden="true" />
        Projects
      </Link>
      <header className="workspace-project-header">
        <div>
          <h1>
            {project.definition.name}
            {project.archived && <Badge variant="outline">Archived</Badge>}
          </h1>
          <p>
            {project.definition.description ||
              "Give this project a clear outcome and bring its work together."}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          {detail.can_manage && (
            <Button
              variant="outline"
              onClick={() => setSettings(true)}
              disabled={commands.locked}
            >
              <Settings2Icon data-icon="inline-start" />
              Settings
            </Button>
          )}
          {detail.can_admin && (
            <Button
              variant="outline"
              disabled={commands.locked}
              onClick={() => setAccess(true)}
            >
              <UsersIcon data-icon="inline-start" />
              Manage access
            </Button>
          )}
          {editable && (
            <Button onClick={() => setEditing("new")}>
              <PlusIcon data-icon="inline-start" />
              Add work
            </Button>
          )}
        </div>
      </header>
      <DraftRecovery
        scope={{ target: { organization }, project: project.id }}
        disabled={!detail.can_manage}
        onResume={({ kind, item }) => {
          if (kind === "settings") setSettings(true);
          if (kind === "properties") setProperties(true);
          if (kind === "item") setEditing({ draft: item });
        }}
      />
      <Tabs
        value={activeView}
        onValueChange={(value) =>
          filters({
            view:
              value === "overview" || value === "history" ? value : undefined,
            pv: value === "overview" || value === "history" ? undefined : value,
            status: undefined,
            repository: undefined,
            layout: undefined,
            issue_type: undefined,
            group_by: undefined,
          })
        }
      >
        <ProjectViewTabs
          project={project}
          active={activeView}
          overview
          target={
            detail.can_manage && !project.archived
              ? { organization }
              : undefined
          }
          editable={editable}
          preserveSelection={() => {
            if (
              !overview &&
              activeView !== "history" &&
              url.searchParams.get("pv") !== activeView
            )
              filters({ pv: activeView });
          }}
        />
        <TabsContent value="history">
          <ProjectHistory
            target={{ organization }}
            project={project.id}
            definition={project.definition}
          />
        </TabsContent>
        <TabsContent value={overview ? "overview" : selected.id}>
          <div className="workspace-project-content">
            <div className="workspace-project-work">
              {resource.error &&
                (after || status || repository || issueType || groupBy) && (
                  <Button
                    variant="outline"
                    onClick={() =>
                      filters({
                        status: "",
                        repository: undefined,
                        issue_type: "",
                        group_by: "none",
                      })
                    }
                  >
                    Reset filters
                  </Button>
                )}
              <Result state={resource}>
                {({ page, can_manage: canManageItems }) =>
                  overview ? (
                    <section className="workspace-project-overview">
                      <h2>About this project</h2>
                      {project.definition.readme ? (
                        <DiscussionMarkdown>
                          {project.definition.readme}
                        </DiscussionMarkdown>
                      ) : (
                        <p className="text-muted-foreground">
                          Describe the outcome, scope, and key decisions in
                          project settings.
                        </p>
                      )}
                      <section
                        className="workspace-project-progress"
                        aria-label="Workflow progress"
                      >
                        <h2>Workflow progress</h2>
                        <div
                          className="workspace-progress-track"
                          role="img"
                          aria-label={project.definition.statuses
                            .map(
                              (status) =>
                                `${status}: ${page.counts[status] ?? 0}`,
                            )
                            .join(", ")}
                        >
                          <span
                            className="workspace-progress-final"
                            style={{
                              width: `${page.total ? ((page.counts[project.definition.statuses.at(-1)!] ?? 0) / page.total) * 100 : 0}%`,
                            }}
                          />
                        </div>
                        <p className="text-sm text-muted-foreground">
                          {page.counts[project.definition.statuses.at(-1)!] ??
                            0}{" "}
                          of {page.total} items in{" "}
                          {project.definition.statuses.at(-1)}
                        </p>
                        <dl>
                          {project.definition.statuses.map((status) => (
                            <div key={status}>
                              <dt>{status}</dt>
                              <dd>{page.counts[status] ?? 0}</dd>
                            </div>
                          ))}
                        </dl>
                      </section>
                    </section>
                  ) : (
                    <>
                      <div className="workspace-project-filters">
                        <FieldSelect
                          label="Layout"
                          value={layout}
                          options={{
                            table: "Table",
                            board: "Board",
                            roadmap: "Roadmap",
                          }}
                          onValueChange={(layout) => filters({ layout })}
                        />
                        <FieldSelect
                          label="Filter by repository"
                          value={repository || "all"}
                          options={{
                            all: "All repositories",
                            ...Object.fromEntries(filterRepositories),
                          }}
                          onOpenChange={(open) => {
                            if (!open) return;
                            if (loadRepositories) repositoryChoices.retry();
                            else setLoadRepositories(true);
                          }}
                          onValueChange={(value) =>
                            filters({
                              repository: value === "all" ? undefined : value,
                            })
                          }
                        />
                        {repositoryChoices.error && (
                          <Button
                            variant="outline"
                            onClick={repositoryChoices.retry}
                          >
                            Retry loading repositories
                          </Button>
                        )}
                        <FieldSelect
                          label="Filter by status"
                          value={status ? `status:${status}` : "all"}
                          options={{
                            all: "All statuses",
                            ...Object.fromEntries(
                              project.definition.statuses.map((status) => [
                                `status:${status}`,
                                status,
                              ]),
                            ),
                          }}
                          onValueChange={(value) =>
                            filters({
                              status: value === "all" ? "" : value.slice(7),
                            })
                          }
                        />
                        <ProjectTypeFilter
                          types={detail.issue_types ?? []}
                          value={issueType}
                          onChange={(value) => filters({ issue_type: value })}
                        />
                        <ProjectGrouping
                          layout={layout}
                          fields={project.definition.fields}
                          issueTypes
                          groupBy={groupBy}
                          onChange={(value) => filters({ group_by: value })}
                        />
                        <span className="text-sm text-muted-foreground">
                          {page.total.toLocaleString()}{" "}
                          {page.total === 1 ? "item" : "items"}
                        </span>
                      </div>
                      {layout === "board" ||
                      page.items.length ||
                      createInGroup ? (
                        <View
                          page={page}
                          viewId={selected.id}
                          editable={editable && canManageItems}
                          bulkTarget={
                            layout !== "roadmap" &&
                            canManageItems &&
                            detail.can_manage &&
                            !project.archived
                              ? { organization }
                              : undefined
                          }
                          onEdit={setEditing}
                          onAddGroupItem={addGroupItem}
                          onAddGroupIssue={
                            canCreateIssue ? addGroupIssue : undefined
                          }
                          showIssueType={selected.show_issue_type}
                          onIssueTypeChange={
                            project.archived
                              ? undefined
                              : () => (after ? filters({}) : resource.retry())
                          }
                          groupBy={groupBy}
                          onChangeItem={changeItem}
                        />
                      ) : (
                        <Empty>
                          <EmptyHeader>
                            <EmptyMedia variant="icon">
                              <FolderKanbanIcon />
                            </EmptyMedia>
                            <EmptyTitle>
                              {issueType && !page.total
                                ? "No work matches this view"
                                : page.total
                                  ? "No items on this page"
                                  : "Bring your work together"}
                            </EmptyTitle>
                            <EmptyDescription>
                              Link repositories, then add existing issues, pull
                              requests, or draft notes.
                            </EmptyDescription>
                          </EmptyHeader>
                          {editable && (
                            <Button onClick={() => setEditing("new")}>
                              <PlusIcon data-icon="inline-start" />
                              Add work
                            </Button>
                          )}
                        </Empty>
                      )}
                      <div className="workspace-pagination">
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
                                "version",
                                String(page.project.version),
                              );
                              if (page.type_cursor)
                                next.searchParams.set(
                                  "type_cursor",
                                  page.type_cursor,
                                );
                              navigate(next.pathname + next.search);
                            }}
                          >
                            More items
                          </Button>
                        )}
                      </div>
                    </>
                  )
                }
              </Result>
            </div>
            <aside
              className="workspace-project-properties"
              aria-label="Project details"
            >
              <section>
                <header>
                  <h2>Properties</h2>
                  {editable && (
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => setProperties(true)}
                    >
                      Edit
                    </Button>
                  )}
                </header>
                <dl>
                  <div>
                    <dt>Status</dt>
                    <dd>
                      <Badge variant="secondary">
                        {statuses[project.properties.status]}
                      </Badge>
                    </dd>
                  </div>
                  <div>
                    <dt>Health</dt>
                    <dd>
                      <Badge
                        variant={
                          project.properties.health === "on_track"
                            ? "outline"
                            : "warning"
                        }
                      >
                        {health[project.properties.health]}
                      </Badge>
                    </dd>
                  </div>
                  <div>
                    <dt>Lead</dt>
                    <dd>
                      {detail.lead?.name ||
                        (project.properties.lead
                          ? "Member without public profile"
                          : "Unassigned")}
                    </dd>
                  </div>
                  <div>
                    <dt>Start date</dt>
                    <dd>{project.properties.start ?? "Not set"}</dd>
                  </div>
                  <div>
                    <dt>Target date</dt>
                    <dd>{project.properties.target ?? "Not set"}</dd>
                  </div>
                </dl>
              </section>
              <Repositories
                organization={organization}
                detail={detail}
                disabled={refreshing}
              />
            </aside>
          </div>
        </TabsContent>
      </Tabs>
      {editing && (
        <WorkspaceItemEditor
          key={draftItem ?? editingRow?.item.id ?? initialItem?.id ?? "new"}
          organization={organization}
          detail={detail}
          row={editingRow}
          draftItem={draftItem}
          initialItem={initialItem}
          initialIssueType={initialIssueType}
          onClose={() => setEditing(undefined)}
        />
      )}
      {properties && (
        <ProjectProperties
          organization={organization}
          detail={detail}
          onClose={() => setProperties(false)}
        />
      )}
      {settings && (
        <ProjectSettings
          target={{ organization }}
          project={project}
          issueTypes
          canArchive={detail.can_admin === true}
          typeChoices={detail.issue_types ?? []}
          onClose={() => setSettings(false)}
        />
      )}
      {access && detail.can_admin && (
        <ProjectAccess
          organization={organization}
          project={project}
          onClose={() => setAccess(false)}
        />
      )}
    </section>
  );
}
