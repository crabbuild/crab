import { AssigneeAvatars } from "@/components/discussion/metadata";
import { useRef, useState } from "react";
import {
  CircleDotIcon,
  FileTextIcon,
  GitPullRequestIcon,
  GitMergeIcon,
  ListTreeIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  PencilIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  NativeSelect,
  NativeSelectOption,
} from "@/components/ui/native-select";
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
import { priorities, workHref, type Row, type ItemPage } from "./model";
export function WorkIcon({ row }: { row: Row }) {
  const Icon = !row.work
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
export function ItemTitle({ repo, row }: { repo: Repository; row: Row }) {
  return (
    <div className="project-item-title">
      <WorkIcon row={row} />
      <div>
        {row.work ? (
          <Link href={workHref(repo, row.work.subject)}>{row.work.title}</Link>
        ) : (
          <strong>{row.item.title}</strong>
        )}
        <div className="project-item-meta">
          {row.work ? (
            <span>
              #{row.work.subject.number} · {row.work.state}
            </span>
          ) : (
            <span>Draft note</span>
          )}
          {row.parent && (
            <Link href={workHref(repo, row.parent.subject)}>
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
export function LinkedItems({ repo, row }: { repo: Repository; row: Row }) {
  return (
    <div className="project-linked-items">
      {row.links.map((link) => (
        <Link
          key={`${link.subject.kind}:${link.subject.number}`}
          href={workHref(repo, link.subject)}
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
        <Link href={workHref(repo, row.work.subject)}>More…</Link>
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
}: {
  repo: Repository;
  page: ItemPage;
  editable: boolean;
  onEdit: (row: Row) => void;
  onStatus: (row: Row, status: string) => void;
}) {
  return (
    <Table className="project-table">
      <TableHeader>
        <TableRow>
          <TableHead>Title</TableHead>
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
            <TableCell>
              {row.assignees.length ? (
                <AssigneeAvatars assignees={row.assignees} />
              ) : (
                <span className="muted">Unassigned</span>
              )}
            </TableCell>
            <TableCell>
              {editable ? (
                <NativeSelect
                  aria-label={`Status for ${row.work?.title ?? row.item.title}`}
                  value={row.item.status}
                  onChange={(e) => onStatus(row, e.target.value)}
                >
                  {page.project.definition.statuses.map((s) => (
                    <NativeSelectOption key={s}>{s}</NativeSelectOption>
                  ))}
                </NativeSelect>
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
}: {
  repo: Repository;
  page: ItemPage;
  editable: boolean;
  onEdit: (row: Row) => void;
  onStatus: (row: Row, status: string) => void;
}) {
  const dragged = useRef<Row | null>(null);
  const [over, setOver] = useState<string>();
  return (
    <div className="project-board" aria-label="Project board">
      {page.project.definition.statuses.map((status) => (
        <section
          key={status}
          className={cn(
            "project-board-column",
            over === status && "project-board-drop",
          )}
          onDragOver={(e) => {
            if (editable && dragged.current) {
              e.preventDefault();
              setOver(status);
            }
          }}
          onDragLeave={() => setOver(undefined)}
          onDrop={(e) => {
            e.preventDefault();
            if (
              editable &&
              dragged.current &&
              dragged.current.item.status !== status
            )
              onStatus(dragged.current, status);
            dragged.current = null;
            setOver(undefined);
          }}
        >
          <header>
            <span className="project-status-dot" />
            <h2>{status}</h2>
            <Badge variant="secondary">{page.counts[status] ?? 0}</Badge>
          </header>
          <div className="project-board-cards">
            {page.items
              .filter((row) => row.item.status === status)
              .map((row) => (
                <article
                  className="project-board-card"
                  key={row.item.id}
                  draggable={editable}
                  onDragStart={() => {
                    dragged.current = row;
                  }}
                  onDragEnd={() => {
                    dragged.current = null;
                    setOver(undefined);
                  }}
                >
                  <ItemTitle repo={repo} row={row} />
                  <footer>
                    <AssigneeAvatars assignees={row.assignees} />
                    <Badge variant="outline">
                      {priorities[row.item.priority]}
                    </Badge>
                    {row.item.end && (
                      <time dateTime={row.item.end}>{row.item.end}</time>
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
            {!page.items.some((row) => row.item.status === status) && (
              <p className="project-column-empty">
                {page.counts[status] ? "No items on this page" : "No items yet"}
              </p>
            )}
          </div>
        </section>
      ))}
    </div>
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
}: {
  repo: Repository;
  page: ItemPage;
  editable: boolean;
  onEdit: (row: Row) => void;
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
                  <ItemTitle repo={repo} row={row} />
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
