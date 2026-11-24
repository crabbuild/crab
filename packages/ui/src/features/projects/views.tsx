import { AssigneeAvatars } from "@/components/discussion/metadata";
import { BlockedBadge } from "@/components/discussion/blocked-badge";
import { IssueTypeBadge } from "@/components/discussion/issue-type-badge";
import { useMutation } from "@/components/discussion/mutations";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { type DragEvent, useRef, useState } from "react";
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
import { endpoint, type Repository } from "@/lib/api";
import { priorities, workHref, type Row, type ItemPage } from "./model";
import { ProjectIssueType } from "./issue-type";
import { useProjectCommands } from "./commands";
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
export function ProjectTable({
  repo,
  page,
  editable,
  onEdit,
  onStatus,
  showIssueType,
  onIssueTypeChange,
}: {
  repo?: Pick<Repository, "id" | "owner" | "name">;
  page: ItemPage;
  editable: boolean;
  onEdit: (row: Row) => void;
  onStatus: (row: Row, status: string) => void;
  showIssueType?: boolean;
  onIssueTypeChange?: () => void;
}) {
  return (
    <Table className="project-table">
      <TableHeader>
        <TableRow>
          <TableHead>Title</TableHead>
          {showIssueType && <TableHead>Type</TableHead>}
          <TableHead>Assignees</TableHead>
          <TableHead>Status</TableHead>
          <TableHead>Priority</TableHead>
          <TableHead>Target date</TableHead>
          {page.project.definition.fields.map((f) => (
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
      <TableBody>
        {page.items.map((row) => (
          <TableRow key={row.item.id}>
            <TableCell>
              <ItemTitle repo={repo} row={row} />
            </TableCell>
            {showIssueType && (
              <TableCell>
                <ProjectIssueType
                  row={row}
                  repo={repo}
                  options={page.issue_types ?? []}
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
                  onValueChange={(status) => onStatus(row, status)}
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
            {page.project.definition.fields.map((f) => (
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
        ))}
      </TableBody>
    </Table>
  );
}
export function ProjectBoard({
  repo,
  page,
  editable,
  onEdit,
  onStatus,
  showIssueType,
  onIssueTypeChange,
}: {
  repo?: Pick<Repository, "id" | "owner" | "name">;
  page: ItemPage;
  editable: boolean;
  onEdit: (row: Row) => void;
  onStatus: (row: Row, status: string) => void;
  showIssueType?: boolean;
  onIssueTypeChange?: () => void;
}) {
  const dragged = useRef<Row | null>(null);
  const [over, setOver] = useState<string>();
  const { csrf } = useProjectCommands();
  const mutation = useMutation(csrf, null);
  const grouped = page.issue_type_counts != null;
  const counts = page.issue_type_counts ?? page.counts;
  const types = page.issue_types ?? [];
  const columns = grouped
    ? [
        ...types.map((type) => ({ key: type.id, name: type.name })),
        { key: "none", name: "No type" },
        ...(counts.other ? [{ key: "other", name: "Other work" }] : []),
      ]
    : page.project.definition.statuses.map((status) => ({
        key: status,
        name: status,
      }));
  const columnFor = (row: Row) =>
    grouped ? (row.issue_type_group ?? "other") : row.item.status;
  const movable = (row: Row) =>
    !row.restricted &&
    (grouped
      ? !!onIssueTypeChange &&
        !!(row.repository ?? repo) &&
        row.issue_type_version != null &&
        !mutation.pending &&
        !mutation.conflict
      : editable);
  async function move(row: Row, column: string) {
    if (!grouped) {
      onStatus(row, column);
      return;
    }
    const source = row.repository ?? repo;
    if (
      !source ||
      row.work?.subject.kind !== "issue" ||
      row.issue_type_version == null
    )
      return;
    const saved = await mutation.run<{ version: number }>(
      endpoint(source, `issues/${row.work.subject.number}/type`),
      "PUT",
      {
        expected_version: row.issue_type_version,
        type_id: column === "none" ? null : column,
      },
      source,
    );
    if (saved) onIssueTypeChange?.();
  }
  return (
    <>
      {mutation.error && (
        <Alert variant="destructive">
          <AlertDescription>{mutation.error}</AlertDescription>
        </Alert>
      )}
      {mutation.conflict && onIssueTypeChange && (
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            mutation.reset();
            onIssueTypeChange();
          }}
        >
          Reload project items
        </Button>
      )}
      <div className="project-board" role="region" aria-label="Project board">
        {columns.map(({ key, name }) => {
          const rows = page.items.filter((row) => columnFor(row) === key);
          const acceptsDrop = (row: Row) =>
            movable(row) &&
            columnFor(row) !== key &&
            (!grouped || key !== "other");
          const allowDrop = (e: DragEvent<HTMLElement>) => {
            if (dragged.current && acceptsDrop(dragged.current)) {
              e.preventDefault();
              e.dataTransfer.dropEffect = "move";
              setOver(key);
            }
          };
          return (
            <section
              key={key}
              aria-label={name}
              className={cn(
                "project-board-column",
                over === key && "project-board-drop",
              )}
              onDragEnter={allowDrop}
              onDragOver={allowDrop}
              onDragLeave={(e) => {
                if (
                  !(e.relatedTarget instanceof Node) ||
                  !e.currentTarget.contains(e.relatedTarget)
                ) {
                  setOver((current) => (current === key ? undefined : current));
                }
              }}
              onDrop={(e) => {
                e.preventDefault();
                if (dragged.current && acceptsDrop(dragged.current))
                  void move(dragged.current, key);
                dragged.current = null;
                setOver(undefined);
              }}
            >
              <header>
                <span className="project-status-dot" />
                <h2>{name}</h2>
                <Badge variant="secondary">{counts[key] ?? 0}</Badge>
              </header>
              <div className="project-board-cards">
                {rows.map((row) => (
                  <article
                    className="project-board-card"
                    key={row.item.id}
                    draggable={movable(row)}
                    onDragStart={(e) => {
                      e.dataTransfer.effectAllowed = "move";
                      e.dataTransfer.setData("text/plain", row.item.id);
                      dragged.current = row;
                    }}
                    onDragEnd={() => {
                      dragged.current = null;
                      setOver(undefined);
                    }}
                  >
                    <ItemTitle
                      repo={repo}
                      row={row}
                      showIssueType={!grouped && showIssueType}
                    />
                    <footer>
                      <AssigneeAvatars assignees={row.assignees} />
                      {grouped && (
                        <Badge variant="secondary">{row.item.status}</Badge>
                      )}
                      <Badge variant="outline">
                        {priorities[row.item.priority]}
                      </Badge>
                      {row.item.end && (
                        <time dateTime={row.item.end}>{row.item.end}</time>
                      )}
                      {grouped && movable(row) && (
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
                    {counts[key] ? "No items on this page" : "No items yet"}
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
const day = 86400000;
function date(value: string) {
  return Date.parse(`${value}T00:00:00Z`);
}
export function ProjectRoadmap({
  repo,
  page,
  editable,
  onEdit,
  showIssueType,
}: {
  repo?: Pick<Repository, "owner" | "name">;
  page: ItemPage;
  editable: boolean;
  onEdit: (row: Row) => void;
  showIssueType?: boolean;
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
          {page.items.map((row) => {
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
            const visible =
              from !== null && to !== null && to > start && from < end;
            const left =
              from === null
                ? 0
                : Math.max(0, ((from - start) / day / days) * 100);
            const right =
              to === null
                ? 0
                : Math.min(100, ((to - start) / day / days) * 100);
            return (
              <div className="project-roadmap-row" role="row" key={row.item.id}>
                <div role="cell">
                  <ItemTitle
                    repo={repo}
                    row={row}
                    showIssueType={showIssueType}
                  />
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
          })}
        </div>
      </div>
    </div>
  );
}
