import { ChevronDownIcon, PencilIcon, TriangleAlertIcon } from "lucide-react";
import { AssigneeAvatars } from "@/components/discussion/metadata";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";
import type { Repository } from "@/lib/api";
import {
  priorities,
  type GroupBy,
  type ItemPage,
  type ProjectTarget,
  type Row,
} from "./model";
import { useProjectBulkChange, useProjectSelection } from "./bulk";
import {
  AddToProjectGroup,
  ProjectGroupMoveFailure,
  projectGroups,
  rowGroup,
  useProjectGroupMove,
  type AddProjectGroupItem,
  type ChangeProjectItem,
} from "./grouping";
import { ItemTitle, LinkedItems } from "./views";
import { ProjectIssueType } from "./issue-type";
import { ColumnLimitControl } from "./column-limit";

export function ProjectBoard({
  repo,
  page,
  viewId,
  editable,
  onEdit,
  groupBy = "status",
  onChangeItem,
  onAddGroupItem,
  onAddGroupIssue,
  showIssueType,
  onIssueTypeChange,
  bulkTarget,
}: {
  repo?: Pick<Repository, "id" | "owner" | "name">;
  page: ItemPage;
  viewId: string;
  editable: boolean;
  onEdit: (row: Row) => void;
  groupBy?: GroupBy;
  onChangeItem: ChangeProjectItem;
  onAddGroupItem?: AddProjectGroupItem;
  onAddGroupIssue?: (typeId: string | null) => void;
  showIssueType?: boolean;
  onIssueTypeChange?: () => void;
  bulkTarget?: ProjectTarget;
}) {
  const byType = groupBy === "issue_type";
  const canSelect = !!bulkTarget && !byType;
  const columns = projectGroups(page, groupBy);
  const savedBoard = page.project.definition.views.find(
    (view) => view.id === viewId && view.layout === "board",
  );
  const limits = savedBoard?.column_limits?.[groupBy];
  const types = page.issue_types ?? [];
  const selection = useProjectSelection(page);
  const { chosen, stale, clear } = selection;
  const apply = useProjectBulkChange(bulkTarget, page.project.id, clear);
  function moveSelected(rows: readonly Row[], key: string) {
    if (!editable || stale) return;
    if (groupBy === "status") void apply(rows, { field: "status", value: key });
    else if (groupBy !== "issue_type")
      void apply(rows, {
        field: "custom",
        id: groupBy.slice("field:".length),
        value: key || null,
      });
  }
  const { draggable, dropTarget, movable, over, error, reload } =
    useProjectGroupMove(
      groupBy,
      repo,
      onIssueTypeChange,
      editable ? onChangeItem : undefined,
      canSelect ? { rows: chosen, stale, move: moveSelected } : undefined,
    );
  return (
    <>
      <ProjectGroupMoveFailure error={error} reload={reload} />
      {canSelect && chosen.length > 0 && (
        <div
          role="group"
          aria-label="Selected Project cards"
          className="flex flex-wrap items-center gap-2 pb-3"
        >
          <span className="text-sm" role="status">
            {chosen.length} selected
          </span>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={!editable || stale}
              >
                Move selected to
                <ChevronDownIcon aria-hidden="true" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start">
              {columns.map(({ key, name }) => (
                <DropdownMenuItem
                  key={key}
                  disabled={chosen.every(
                    (row) => rowGroup(row, groupBy) === key,
                  )}
                  onSelect={() => moveSelected(chosen, key)}
                >
                  {name}
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={!editable}
            onClick={clear}
          >
            Clear selection
          </Button>
          {stale && (
            <span role="status">Cards changed. Select them again.</span>
          )}
        </div>
      )}
      <div className="project-board" role="region" aria-label="Project board">
        {columns.map(({ key, name, count, rows }) => {
          const column = selection.groupState(rows);
          const limit =
            limits && Object.hasOwn(limits, key) ? limits[key] : undefined;
          const overLimit = limit !== undefined && count > limit;
          return (
            <section
              key={key}
              aria-label={name}
              className={cn(
                "project-board-column",
                over === key && "project-board-drop",
              )}
              {...dropTarget(key)}
            >
              <header>
                {canSelect && !column.empty ? (
                  <Checkbox
                    aria-label={`Select all editable cards in ${name}`}
                    checked={column.checked}
                    disabled={!editable}
                    onCheckedChange={(checked) =>
                      selection.toggleGroup(rows, checked === true)
                    }
                  />
                ) : (
                  <span className="project-status-dot" />
                )}
                <h2>{name}</h2>
                <Badge variant={overLimit ? "warning" : "secondary"}>
                  {overLimit && <TriangleAlertIcon aria-hidden="true" />}
                  {limit === undefined ? (
                    count
                  ) : (
                    <>
                      <span aria-hidden="true">
                        {count} / {limit}
                      </span>
                      <span className="sr-only">
                        {count} items, limit {limit}
                        {overLimit ? ", over limit" : ""}
                      </span>
                    </>
                  )}
                </Badge>
                <AddToProjectGroup
                  grouping={groupBy}
                  value={key}
                  name={name}
                  onAdd={editable ? onAddGroupItem : undefined}
                  onCreateIssue={editable ? onAddGroupIssue : undefined}
                />
                {bulkTarget && savedBoard && (
                  <ColumnLimitControl
                    key={`${savedBoard.id}:${groupBy}`}
                    target={bulkTarget}
                    project={page.project}
                    column={{
                      view: savedBoard.id,
                      group_by: groupBy,
                      value: key,
                    }}
                    name={name}
                    limit={limit}
                    editable={editable}
                  />
                )}
              </header>
              <div className="project-board-cards">
                {rows.map((row) => (
                  <article
                    className="project-board-card"
                    key={row.item.id}
                    data-state={
                      canSelect && selection.checked(row)
                        ? "selected"
                        : undefined
                    }
                    {...draggable(row)}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <ItemTitle
                        repo={repo}
                        row={row}
                        showIssueType={!byType && showIssueType}
                      />
                      {canSelect && !row.restricted && (
                        <Checkbox
                          aria-label={`Select ${row.work?.title ?? row.item.title}`}
                          className="mt-0.5"
                          checked={selection.checked(row)}
                          disabled={!editable}
                          onCheckedChange={(checked) =>
                            selection.toggle(row, checked === true)
                          }
                        />
                      )}
                    </div>
                    <footer>
                      <AssigneeAvatars assignees={row.assignees} />
                      {groupBy !== "status" && (
                        <Badge variant="secondary">{row.item.status}</Badge>
                      )}
                      <Badge variant="outline">
                        {priorities[row.item.priority]}
                      </Badge>
                      {row.item.end && (
                        <time dateTime={row.item.end}>{row.item.end}</time>
                      )}
                      {byType && movable(row) && (
                        <ProjectIssueType
                          row={row}
                          repo={repo}
                          options={types}
                          onChange={onIssueTypeChange}
                        />
                      )}
                      {editable && (
                        <Button
                          size="icon-sm"
                          variant="ghost"
                          aria-label={`Edit ${row.work?.title ?? row.item.title}`}
                          onClick={() => onEdit(row)}
                        >
                          <PencilIcon />
                        </Button>
                      )}
                    </footer>
                    <LinkedItems repo={repo} row={row} />
                  </article>
                ))}
                {!rows.length && (
                  <p className="project-column-empty">
                    {count ? "No items on this page" : "No items yet"}
                  </p>
                )}
              </div>
            </section>
          );
        })}
      </div>
    </>
  );
}
