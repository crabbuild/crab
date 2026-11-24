import { AssigneeAvatars } from "@/components/discussion/metadata";
import { BlockedBadge } from "@/components/discussion/blocked-badge";
import { IssueTypeBadge } from "@/components/discussion/issue-type-badge";
import { useState } from "react";
import {
  CircleDotIcon,
  FileTextIcon,
  GitPullRequestIcon,
  GitMergeIcon,
  ListTreeIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  PencilIcon,
  LockKeyholeIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableHeader,
  TableHead,
  TableBody,
  TableRow,
  TableCell,
} from "@/components/ui/table";
import { Link } from "@/components/navigation/link";
import { cn } from "@/lib/utils";
import type { Repository } from "@/lib/api";
import {
  priorities,
  workHref,
  type Row,
  type ItemPage,
  type GroupBy,
} from "./model";
import { ProjectIssueType } from "./issue-type";
import {
  projectGroups,
  AddToProjectGroup,
  type AddProjectGroupItem,
  type ChangeProjectItem,
  ProjectGroupMoveFailure,
  useProjectGroupMove,
} from "./grouping";
export function WorkIcon({ row }: { row: Row }) {
  const Icon = row.restricted
    ? LockKeyholeIcon
    : !row.work
      ? FileTextIcon
      : row.work.subject.kind === "issue"
        ? CircleDotIcon
        : row.work.state === "merged"
          ? GitMergeIcon
          : GitPullRequestIcon;
  return (
    <Icon
      className={cn(
        "project-work-icon",
        row.work && `project-state-${row.work.state}`,
      )}
      aria-hidden="true"
    />
  );
}
export function ItemTitle({
  repo,
  row,
  showIssueType,
}: {
  repo?: Pick<Repository, "owner" | "name">;
  row: Row;
  showIssueType?: boolean;
}) {
  const source = row.repository ?? repo;
  return (
    <div className="project-item-title">
      <WorkIcon row={row} />
      <div>
        {row.work && source ? (
          <Link href={workHref(source, row.work.subject)}>
            {row.work.title}
          </Link>
        ) : (
          <strong>{row.item.title}</strong>
        )}
        <div className="project-item-meta">
          {row.work ? (
            <span>
              {row.repository &&
                `${row.repository.owner}/${row.repository.name} `}
              #{row.work.subject.number} · {row.work.state}
            </span>
          ) : (
            <span>
              {row.restricted ? "Repository access required" : "Draft note"}
            </span>
          )}
          {row.blocked && !row.restricted && <BlockedBadge />}
          {showIssueType && !row.restricted && row.issue_type && (
            <IssueTypeBadge type={row.issue_type} />
          )}
          {row.parent && source && (
            <Link href={workHref(source, row.parent.subject)}>
              Sub-issue of #{row.parent.subject.number}
            </Link>
          )}
          {!!row.progress?.total && (
            <span>
              <ListTreeIcon aria-hidden="true" />
              {row.progress.closed}/{row.progress.total}
            </span>
          )}
        </div>
      </div>
    </div>
  );
}
export function LinkedItems({
  repo,
  row,
}: {
  repo?: Pick<Repository, "owner" | "name">;
  row: Row;
}) {
  const source = row.repository ?? repo;
  if (!source) return null;
  return (
    <div className="project-linked-items">
      {row.links.map((link) => (
        <Link
          key={`${link.subject.kind}:${link.subject.number}`}
          href={workHref(source, link.subject)}
          title={link.title}
        >
          {link.subject.kind === "pull" ? (
            <GitPullRequestIcon aria-hidden="true" />
          ) : (
            <CircleDotIcon aria-hidden="true" />
          )}
          #{link.subject.number}
        </Link>
      ))}
      {row.more_links && row.work && (
        <Link href={workHref(source, row.work.subject)}>More…</Link>
      )}
    </div>
  );
}
function ProjectGroupLabel({ name, count }: { name: string; count: number }) {
  return (
    <span className="inline-flex items-center gap-2">
      <span className="max-w-[min(36ch,65vw)] truncate" title={name}>
        {name}
      </span>
      <Badge variant="outline">
        {count}
        <span className="sr-only">{count === 1 ? " item" : " items"}</span>
      </Badge>
    </span>
  );
}

export function ProjectTable({
  repo,
  page,
  editable,
  onEdit,
  groupBy,
  onChangeItem,
  onAddGroupItem,
  onAddGroupIssue,
  showIssueType,
  onIssueTypeChange,
}: {
  repo?: Pick<Repository, "id" | "owner" | "name">;
  page: ItemPage;
  editable: boolean;
  onEdit: (row: Row) => void;
  groupBy?: GroupBy;
  onChangeItem: ChangeProjectItem;
  onAddGroupItem?: AddProjectGroupItem;
  onAddGroupIssue?: (typeId: string | null) => void;
  showIssueType?: boolean;
  onIssueTypeChange?: () => void;
}) {
  const groups = groupBy ? projectGroups(page, groupBy) : undefined;
  const showType = groupBy === "issue_type" || showIssueType;
  const fields = page.project.definition.fields;
  const types = page.issue_types ?? [];
  const columns = 6 + fields.length + Number(!!showType) + Number(editable);
  const { draggable, dropTarget, over, error, reload } = useProjectGroupMove(
    groupBy,
    repo,
    onIssueTypeChange,
    editable ? onChangeItem : undefined,
  );
  const renderRow = (row: Row) => (
    <TableRow key={row.item.id} {...draggable(row)}>
      <TableCell>
        <ItemTitle repo={repo} row={row} />
      </TableCell>
      {showType && (
        <TableCell>
          <ProjectIssueType
            row={row}
            repo={repo}
            options={types}
            onChange={onIssueTypeChange}
          />
        </TableCell>
      )}
      <TableCell>
        {row.assignees.length ? (
          <AssigneeAvatars assignees={row.assignees} />
        ) : (
          <span className="muted">Unassigned</span>
        )}
      </TableCell>
      <TableCell>
        {editable && !row.restricted ? (
          <Select
            value={row.item.status}
            onValueChange={(status) =>
              onChangeItem(row, (item) => ({ ...item, status }))
            }
          >
            <SelectTrigger
              aria-label={`Status for ${row.work?.title ?? row.item.title}`}
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectGroup>
                {page.project.definition.statuses.map((status) => (
                  <SelectItem key={status} value={status}>
                    {status}
                  </SelectItem>
                ))}
              </SelectGroup>
            </SelectContent>
          </Select>
        ) : (
          <Badge variant="secondary">{row.item.status}</Badge>
        )}
      </TableCell>
      <TableCell>{priorities[row.item.priority]}</TableCell>
      <TableCell>{row.item.end ?? "—"}</TableCell>
      {fields.map((f) => (
        <TableCell key={f.id}>{row.item.values[f.id] ?? "—"}</TableCell>
      ))}
      <TableCell>
        <LinkedItems repo={repo} row={row} />
      </TableCell>
      {editable && (
        <TableCell>
          <Button
            size="icon-sm"
            variant="ghost"
            aria-label={`Edit ${row.work?.title ?? row.item.title}`}
            onClick={() => onEdit(row)}
          >
            <PencilIcon />
          </Button>
        </TableCell>
      )}
    </TableRow>
  );

  return (
    <>
      <ProjectGroupMoveFailure error={error} reload={reload} />
      <Table className="project-table">
        <TableHeader>
          <TableRow>
            <TableHead>Title</TableHead>
            {showType && <TableHead>Type</TableHead>}
            <TableHead>Assignees</TableHead>
            <TableHead>Status</TableHead>
            <TableHead>Priority</TableHead>
            <TableHead>Target date</TableHead>
            {fields.map((f) => (
              <TableHead key={f.id}>{f.name}</TableHead>
            ))}
            <TableHead>Linked work</TableHead>
            {editable && (
              <TableHead>
                <span className="sr-only">Actions</span>
              </TableHead>
            )}
          </TableRow>
        </TableHeader>
        {groups ? (
          groups.map(({ key, name, count, rows }) => (
            <TableBody
              key={key}
              aria-label={name}
              className={cn(over === key && "project-group-drop")}
              {...dropTarget(key)}
            >
              <TableRow>
                <TableHead
                  scope="rowgroup"
                  colSpan={columns}
                  className="project-group-heading"
                >
                  <div className="flex items-center gap-2">
                    <ProjectGroupLabel name={name} count={count} />
                    <AddToProjectGroup
                      grouping={groupBy}
                      value={key}
                      name={name}
                      onAdd={editable ? onAddGroupItem : undefined}
                      onCreateIssue={editable ? onAddGroupIssue : undefined}
                    />
                  </div>
                </TableHead>
              </TableRow>
              {rows.map(renderRow)}
            </TableBody>
          ))
        ) : (
          <TableBody>{page.items.map(renderRow)}</TableBody>
        )}
      </Table>
    </>
  );
}
export function ProjectBoard({
  repo,
  page,
  editable,
  onEdit,
  groupBy = "status",
  onChangeItem,
  onAddGroupItem,
  onAddGroupIssue,
  showIssueType,
  onIssueTypeChange,
}: {
  repo?: Pick<Repository, "id" | "owner" | "name">;
  page: ItemPage;
  editable: boolean;
  onEdit: (row: Row) => void;
  groupBy?: GroupBy;
  onChangeItem: ChangeProjectItem;
  onAddGroupItem?: AddProjectGroupItem;
  onAddGroupIssue?: (typeId: string | null) => void;
  showIssueType?: boolean;
  onIssueTypeChange?: () => void;
}) {
  const byType = groupBy === "issue_type";
  const columns = projectGroups(page, groupBy);
  const types = page.issue_types ?? [];
  const { draggable, dropTarget, movable, over, error, reload } =
    useProjectGroupMove(
      groupBy,
      repo,
      onIssueTypeChange,
      editable ? onChangeItem : undefined,
    );
  return (
    <>
      <ProjectGroupMoveFailure error={error} reload={reload} />
      <div className="project-board" role="region" aria-label="Project board">
        {columns.map(({ key, name, count, rows }) => (
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
              <span className="project-status-dot" />
              <h2>{name}</h2>
              <Badge variant="secondary">{count}</Badge>
              <AddToProjectGroup
                grouping={groupBy}
                value={key}
                name={name}
                onAdd={editable ? onAddGroupItem : undefined}
                onCreateIssue={editable ? onAddGroupIssue : undefined}
              />
            </header>
            <div className="project-board-cards">
              {rows.map((row) => (
                <article
                  className="project-board-card"
                  key={row.item.id}
                  {...draggable(row)}
                >
                  <ItemTitle
                    repo={repo}
                    row={row}
                    showIssueType={!byType && showIssueType}
                  />
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
        ))}
      </div>
    </>
  );
}
const day = 86400000;
function date(value: string) {
  return Date.parse(`${value}T00:00:00Z`);
}
export function ProjectRoadmap({
  repo,
  page,
  editable,
  onEdit,
  groupBy,
  onAddGroupItem,
  onAddGroupIssue,
  showIssueType,
  onIssueTypeChange,
}: {
  repo?: Pick<Repository, "id" | "owner" | "name">;
  page: ItemPage;
  editable: boolean;
  onEdit: (row: Row) => void;
  groupBy?: GroupBy;
  onAddGroupItem?: AddProjectGroupItem;
  onAddGroupIssue?: (typeId: string | null) => void;
  showIssueType?: boolean;
  onIssueTypeChange?: () => void;
}) {
  const [offset, setOffset] = useState(0);
  const today = new Date();
  const start = Date.UTC(
    today.getUTCFullYear(),
    today.getUTCMonth() + offset,
    1,
  );
  const end = Date.UTC(
    today.getUTCFullYear(),
    today.getUTCMonth() + offset + 3,
    1,
  );
  const days = (end - start) / day;
  const months = [0, 1, 2].map(
    (i) =>
      new Date(
        Date.UTC(today.getUTCFullYear(), today.getUTCMonth() + offset + i, 1),
      ),
  );
  const groups = groupBy ? projectGroups(page, groupBy) : undefined;
  const byType = groupBy === "issue_type";
  const types = page.issue_types ?? [];
  const renderRow = (row: Row) => {
    const from = row.item.start
      ? date(row.item.start)
      : row.item.end
        ? date(row.item.end)
        : null;
    const to = row.item.end
      ? date(row.item.end) + day
      : from === null
        ? null
        : from + day;
    const visible = from !== null && to !== null && to > start && from < end;
    const left =
      from === null ? 0 : Math.max(0, ((from - start) / day / days) * 100);
    const right =
      to === null ? 0 : Math.min(100, ((to - start) / day / days) * 100);
    return (
      <div className="project-roadmap-row" role="row" key={row.item.id}>
        <div role="cell">
          <ItemTitle
            repo={repo}
            row={row}
            showIssueType={!byType && showIssueType}
          />
          {byType && onIssueTypeChange && row.issue_type_version != null && (
            <ProjectIssueType
              row={row}
              repo={repo}
              options={types}
              onChange={onIssueTypeChange}
            />
          )}
        </div>
        <div
          className="project-roadmap-track"
          role="cell"
          style={{ backgroundSize: `${100 / days}% 100%` }}
        >
          {visible ? (
            <Button
              className="project-roadmap-bar"
              variant="secondary"
              style={{
                left: `${left}%`,
                width: `${Math.max(right - left, 1)}%`,
              }}
              disabled={!editable}
              aria-label={`Schedule ${row.work?.title ?? row.item.title}: ${row.item.start ?? row.item.end} to ${row.item.end ?? row.item.start}`}
              onClick={() => onEdit(row)}
            >
              {row.item.status}
            </Button>
          ) : (
            <Button
              size="sm"
              variant="ghost"
              disabled={!editable}
              onClick={() => onEdit(row)}
            >
              {from === null
                ? "Set dates"
                : `${row.item.start ?? row.item.end} · outside this quarter`}
            </Button>
          )}
        </div>
      </div>
    );
  };
  return (
    <div className="project-roadmap">
      <div className="project-roadmap-controls">
        <strong>
          {months[0].toLocaleDateString(undefined, {
            month: "long",
            year: "numeric",
            timeZone: "UTC",
          })}{" "}
          –{" "}
          {months[2].toLocaleDateString(undefined, {
            month: "long",
            timeZone: "UTC",
          })}
        </strong>
        <div className="flex gap-1">
          <Button variant="outline" size="sm" onClick={() => setOffset(0)}>
            Today
          </Button>
          <Button
            variant="outline"
            size="icon-sm"
            aria-label="Previous quarter"
            onClick={() => setOffset((n) => n - 3)}
          >
            <ChevronLeftIcon />
          </Button>
          <Button
            variant="outline"
            size="icon-sm"
            aria-label="Next quarter"
            onClick={() => setOffset((n) => n + 3)}
          >
            <ChevronRightIcon />
          </Button>
        </div>
      </div>
      <div className="project-roadmap-scroll">
        <div
          className="project-roadmap-grid"
          role="table"
          aria-label="Project roadmap"
        >
          <div className="project-roadmap-heading" role="row">
            <div role="columnheader">Work item</div>
            <div
              className="project-roadmap-months"
              role="columnheader"
              style={{
                gridTemplateColumns: months
                  .map(
                    (month) =>
                      `${(Date.UTC(month.getUTCFullYear(), month.getUTCMonth() + 1, 1) - month.getTime()) / day}fr`,
                  )
                  .join(" "),
              }}
            >
              {months.map((month) => (
                <span key={month.toISOString()}>
                  {month.toLocaleDateString(undefined, {
                    month: "long",
                    timeZone: "UTC",
                  })}
                </span>
              ))}
            </div>
          </div>
          {groups
            ? groups
                .filter(
                  ({ count }) =>
                    count > 0 ||
                    (!page.items.length &&
                      editable &&
                      (byType ? !!onAddGroupIssue : !!onAddGroupItem)),
                )
                .map(({ key, name, count, rows }) => (
                  <div key={key} role="rowgroup" aria-label={name}>
                    <div
                      className="project-group-heading project-roadmap-group"
                      role="row"
                    >
                      <div role="cell" aria-colspan={2}>
                        <div className="flex items-center gap-2">
                          <ProjectGroupLabel name={name} count={count} />
                          <AddToProjectGroup
                            grouping={groupBy}
                            value={key}
                            name={name}
                            onAdd={editable ? onAddGroupItem : undefined}
                            onCreateIssue={
                              editable ? onAddGroupIssue : undefined
                            }
                          />
                        </div>
                      </div>
                    </div>
                    {rows.map(renderRow)}
                  </div>
                ))
            : page.items.map(renderRow)}
        </div>
      </div>
    </div>
  );
}
