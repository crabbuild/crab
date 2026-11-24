import { FieldSelect } from "@/components/ui/field-select";
import { useEffect, useRef, useState } from "react";
import { Link } from "@/components/navigation/link";
import { Result } from "@/components/request/result";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Empty, EmptyHeader, EmptyTitle } from "@/components/ui/empty";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { navigate, useRequest } from "@/lib/api";
import type { AuditAction, AuditEvent, AuditPage } from "@/lib/contracts";
import { teamHref } from "./teams/model";

const actions: Record<AuditAction, string> = {
  organization_created: "Organization created",
  automation_changed: "Automation identity changed",
  repository_properties_changed: "Repository properties changed",
  issue_types_changed: "Issue types changed",
  organization_edited: "Organization updated",
  organization_membership: "Organization membership changed",
  organization_invitation: "Organization invitation",
  rulesets_changed: "Rulesets changed",
  project_changed: "Project changed",
  team_created: "Team created",
  team_edited: "Team updated",
  team_group_sync: "Identity provider group sync",
  team_deleted: "Team deleted",
  team_membership: "Team membership changed",
  team_creation_policy: "Team creation policy changed",
  repository_policy_changed: "Repository policy changed",
  project_policy_changed: "Project default access changed",
  repository_registered: "Repository registered",
  repository_access_changed: "Repository access changed",
  outside_collaborators_removed: "Outside collaborators removed",
  repository_transferred_in: "Repository transferred in",
  repository_transferred_out: "Repository transferred out",
};

const textFilters = [
  { name: "actor", label: "Actor account ID", length: 36 },
  { name: "account", label: "Affected account ID", length: 36 },
  { name: "repository", label: "Repository name", length: 100 },
] as const;
const dates = [
  { name: "from", label: "From (UTC)" },
  { name: "through", label: "Through (UTC)" },
] as const;
const dateFormat = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "medium",
  timeZone: "UTC",
});

function queryFor(filters: URLSearchParams) {
  const query = new URLSearchParams();
  for (const name of ["action", ...textFilters.map(({ name }) => name)]) {
    const value = filters.get(name);
    if (value) query.set(name, value);
  }
  for (const [name, parameter, offset] of [
    ["from", "since_ms", 0],
    ["through", "until_ms", 86_400_000],
  ] as const) {
    const date = filters.get(name);
    const time = date ? Date.parse(`${date}T00:00:00Z`) : NaN;
    if (Number.isFinite(time) && time >= 0) {
      query.set(parameter, String(time + offset));
    }
  }
  return query;
}

export function OrganizationAuditLog({ id, url }: { id: string; url: URL }) {
  const root = `/-/organizations/${id}`;
  const filters = url.searchParams;
  const serialized = filters.toString();
  const query = queryFor(filters);
  return (
    <section className="grid gap-5" aria-labelledby="organization-audit">
      <div>
        <h2 id="organization-audit" className="text-lg font-semibold">
          Audit log
        </h2>
        <p className="text-sm text-muted-foreground">
          Organization, team, and repository access events. Timestamps use UTC.
        </p>
      </div>
      <form
        key={serialized}
        onSubmit={(event) => {
          event.preventDefault();
          const values = new FormData(event.currentTarget);
          const params = new URLSearchParams({ tab: "audit" });
          for (const name of [
            "action",
            ...textFilters.map(({ name }) => name),
            ...dates.map(({ name }) => name),
          ]) {
            const value = String(values.get(name) ?? "").trim();
            if (value && (name !== "action" || value !== "all")) {
              params.set(name, value);
            }
          }
          navigate(`${root}?${params}`);
        }}
      >
        <FieldGroup className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          <Field>
            <FieldLabel htmlFor="audit-action">Action</FieldLabel>
            <FieldSelect
              id="audit-action"
              className="w-full"
              name="action"
              defaultValue={filters.get("action") ?? "all"}
              options={[
                { value: "all", label: "All actions" },
                ...Object.entries(actions).map(([value, label]) => ({
                  value: value,
                  label: label,
                })),
              ]}
            />
          </Field>
          {textFilters.map(({ name, label, length }) => (
            <Field key={name}>
              <FieldLabel htmlFor={`audit-${name}`}>{label}</FieldLabel>
              <Input
                id={`audit-${name}`}
                name={name}
                maxLength={length}
                defaultValue={filters.get(name) ?? ""}
                autoComplete="off"
              />
            </Field>
          ))}
          {dates.map(({ name, label }) => (
            <Field key={name}>
              <FieldLabel htmlFor={`audit-${name}`}>{label}</FieldLabel>
              <Input
                id={`audit-${name}`}
                type="date"
                name={name}
                min="1970-01-01"
                defaultValue={filters.get(name) ?? ""}
              />
            </Field>
          ))}
        </FieldGroup>
        <div className="mt-3 flex flex-wrap gap-2">
          <Button type="submit">Apply filters</Button>
          {query.size > 0 && (
            <Button asChild variant="ghost">
              <Link href={`${root}?tab=audit`}>Clear filters</Link>
            </Button>
          )}
        </div>
      </form>
      <AuditResults id={id} query={query.toString()} key={serialized} />
    </section>
  );
}

function AuditResults({ id, query }: { id: string; query: string }) {
  const [cursor, setCursor] = useState<string>();
  const root = `/api/organizations/${id}/audit-log`;
  const params = new URLSearchParams(query);
  if (cursor) params.set("after", cursor);
  const request = useRequest<AuditPage>(`${root}?${params}`);
  return (
    <>
      <div className="flex flex-wrap items-start gap-2">
        <Button
          variant="outline"
          onClick={() => (cursor ? setCursor(undefined) : request.retry())}
        >
          {cursor ? "First page" : "Refresh audit log"}
        </Button>
        <AuditExport root={root} query={query} />
      </div>
      <Result state={request} timingLabel="Organization audit log">
        {({ items, next }) => (
          <>
            {items.length ? (
              <Table aria-label="Audit events">
                <TableHeader>
                  <TableRow>
                    <TableHead>Event</TableHead>
                    <TableHead>Actor</TableHead>
                    <TableHead>Target</TableHead>
                    <TableHead>Time (UTC)</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {items.map((event) => (
                    <AuditRow key={event.id} event={event} organization={id} />
                  ))}
                </TableBody>
              </Table>
            ) : (
              <Empty>
                <EmptyHeader>
                  <EmptyTitle>
                    {next
                      ? "No matching events on this page"
                      : "No matching audit events"}
                  </EmptyTitle>
                </EmptyHeader>
              </Empty>
            )}
            {next && (
              <Button
                variant="outline"
                className="justify-self-start"
                onClick={() => setCursor(next)}
              >
                Next page
              </Button>
            )}
          </>
        )}
      </Result>
    </>
  );
}

function AuditRow({
  event,
  organization,
}: {
  event: AuditEvent;
  organization: string;
}) {
  const time = new Date(event.occurred_at_ms);
  return (
    <TableRow>
      <TableCell className="min-w-44 whitespace-normal">
        <div className="font-medium">{actions[event.action]}</div>
        {event.detail && (
          <div className="text-muted-foreground">{event.detail}</div>
        )}
      </TableCell>
      <TableCell className="whitespace-normal">
        {event.actor ? (
          <Link
            className="wrap-anywhere text-link hover:underline"
            href={`/-/users/${event.actor}`}
          >
            {event.actor_profile?.handle
              ? `@${event.actor_profile.handle}`
              : event.actor}
          </Link>
        ) : event.operator ? (
          event.action === "team_group_sync" ? (
            "SCIM"
          ) : (
            "Operator"
          )
        ) : (
          "Account unavailable"
        )}
      </TableCell>
      <TableCell className="min-w-32 whitespace-normal">
        {event.repository && (
          <span className="wrap-anywhere">{event.repository.name}</span>
        )}
        {event.team && (
          <Link
            href={teamHref(organization, event.team)}
            className="block wrap-anywhere text-link hover:underline"
          >
            Team {event.team}
          </Link>
        )}
        {event.account && (
          <Link
            href={`/-/users/${event.account}`}
            className="block wrap-anywhere text-link hover:underline"
          >
            Account {event.account}
          </Link>
        )}
        {event.project && (
          <span className="wrap-anywhere">Project {event.project}</span>
        )}
        {!event.repository &&
          !event.team &&
          !event.account &&
          !event.project &&
          "Organization"}
      </TableCell>
      <TableCell>
        <time dateTime={time.toISOString()}>{dateFormat.format(time)}</time>
      </TableCell>
    </TableRow>
  );
}

type Format = "json" | "csv";

function AuditExport({ root, query }: { root: string; query: string }) {
  const controller = useRef<AbortController | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [continuation, setContinuation] = useState<{
    cursor: string;
    format: Format;
    part: number;
  }>();
  const [notice, setNotice] = useState("");
  useEffect(() => () => controller.current?.abort(), []);

  async function download(format: Format, part: number, cursor?: string) {
    const active = new AbortController();
    controller.current = active;
    setBusy(true);
    setError("");
    const params = new URLSearchParams(query);
    params.set("export", format);
    if (cursor) params.set("after", cursor);
    try {
      const response = await fetch(`${root}?${params}`, {
        signal: active.signal,
      });
      if (!response.ok) {
        const problem: { error?: { message?: string } } = await response.json();
        throw new Error(problem.error?.message ?? "The audit export failed.");
      }
      const next = response.headers.get("X-Crab-Audit-Next");
      const count = Number(response.headers.get("X-Crab-Audit-Count"));
      const blob = await response.blob();
      if (active.signal.aborted) return;
      if (count > 0 || !next) {
        const href = URL.createObjectURL(blob);
        const anchor = document.createElement("a");
        anchor.href = href;
        anchor.download = `crab-audit-log-${part}.${format}`;
        anchor.click();
        window.setTimeout(() => URL.revokeObjectURL(href), 0);
      }
      setContinuation(
        next ? { cursor: next, format, part: part + 1 } : undefined,
      );
      setNotice(
        next
          ? count > 0
            ? `Part ${part} downloaded. More history is available.`
            : "No matching events in this part. More history is available."
          : "Audit export complete.",
      );
    } catch (error) {
      if (!active.signal.aborted) {
        setError(
          error instanceof Error ? error.message : "The audit export failed.",
        );
      }
    } finally {
      if (!active.signal.aborted) setBusy(false);
    }
  }

  return (
    <div className="grid gap-2">
      <div className="flex flex-wrap gap-2">
        <Button
          variant="outline"
          disabled={busy}
          onClick={() => void download("json", 1)}
        >
          Export JSON
        </Button>
        <Button
          variant="outline"
          disabled={busy}
          onClick={() => void download("csv", 1)}
        >
          Export CSV
        </Button>
        {continuation && (
          <Button
            variant="outline"
            disabled={busy}
            onClick={() =>
              void download(
                continuation.format,
                continuation.part,
                continuation.cursor,
              )
            }
          >
            Export next part
          </Button>
        )}
      </div>
      {error && (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}
      {notice && (
        <p role="status" className="text-sm">
          {notice}
        </p>
      )}
    </div>
  );
}
