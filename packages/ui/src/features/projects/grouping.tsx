import { useRef, useState, type DragEvent } from "react";
import { PlusIcon } from "lucide-react";
import { useMutation } from "@/components/discussion/mutations";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { FieldSelect } from "@/components/ui/field-select";
import { endpoint, type Repository } from "@/lib/api";
import { useProjectCommands } from "./commands";
import type { Field, GroupBy, Item, ItemPage, Layout, Row } from "./model";

function layoutFields(layout: Layout, fields: Field[]) {
  return layout === "board"
    ? fields.filter((field) => field.definition.kind === "select")
    : fields;
}

export function selectedGrouping(
  layout: Layout,
  fields: Field[],
  issueTypes: boolean,
  requested: string | null | undefined,
): GroupBy | undefined {
  if (requested === "status" || (requested === "issue_type" && issueTypes))
    return requested;
  const field = layoutFields(layout, fields).find(
    (field) => requested === `field:${field.id}`,
  );
  if (field) return `field:${field.id}`;
  return layout === "board" ? "status" : undefined;
}

export function ProjectGrouping({
  layout,
  fields,
  issueTypes,
  groupBy,
  onChange,
  label,
  id,
}: {
  layout: Layout;
  fields: Field[];
  issueTypes: boolean;
  groupBy: GroupBy | undefined;
  onChange: (value: string) => void;
  label?: string;
  id?: string;
}) {
  return (
    <FieldSelect
      id={id}
      label={label ?? (layout === "board" ? "Group board by" : "Group by")}
      value={groupBy ?? "none"}
      options={{
        ...(layout === "board" ? {} : { none: "No grouping" }),
        status: "Workflow status",
        ...(issueTypes ? { issue_type: "Issue type" } : {}),
        ...Object.fromEntries(
          layoutFields(layout, fields).map((field) => [
            `field:${field.id}`,
            field.name || "Unnamed field",
          ]),
        ),
      }}
      onValueChange={onChange}
    />
  );
}

function rowGroup(row: Row, grouping: GroupBy) {
  if (grouping === "status") return row.item.status;
  if (grouping === "issue_type") return row.issue_type_group ?? "other";
  return row.item.values[grouping.slice("field:".length)] ?? "";
}

export function projectGroups(page: ItemPage, grouping: GroupBy) {
  const { definition } = page.project;
  const counts =
    page.group_counts ?? (grouping === "status" ? page.counts : {});
  const field = definition.fields.find(
    (field) => grouping === `field:${field.id}`,
  );
  let names: { key: string; name: string }[];
  if (grouping === "status") {
    names = definition.statuses.map((status) => ({
      key: status,
      name: status,
    }));
  } else if (grouping === "issue_type") {
    names = (page.issue_types ?? []).map((type) => ({
      key: type.id,
      name: type.name,
    }));
    names.push({ key: "none", name: "No type" });
    if (counts.other) names.push({ key: "other", name: "Other work" });
  } else {
    const values =
      field?.definition.kind === "select"
        ? field.definition.options
        : Object.keys(counts)
            .filter(Boolean)
            .sort((a, b) =>
              field?.definition.kind === "number"
                ? Number(a) - Number(b)
                : a.localeCompare(b),
            );
    names = values.map((value) => ({ key: value, name: value }));
    names.push({ key: "", name: "No value" });
  }
  const groups = names.map(({ key, name }) => ({
    key,
    name,
    count: Object.hasOwn(counts, key) ? counts[key] : 0,
    rows: [] as Row[],
  }));
  const byKey = new Map(groups.map((group) => [group.key, group]));
  for (const row of page.items)
    byKey.get(rowGroup(row, grouping))?.rows.push(row);
  return groups;
}

export type ChangeProjectItem = (row: Row, edit: (item: Item) => Item) => void;
type ProjectOwnedGroup = Exclude<GroupBy, "issue_type">;
export type AddProjectGroupItem = (
  group: ProjectOwnedGroup,
  key: string,
) => void;

export function applyProjectGroup(
  item: Item,
  group: ProjectOwnedGroup,
  key: string,
): Item {
  if (group === "status") return { ...item, status: key };
  const field = group.slice("field:".length);
  const values = { ...item.values };
  if (key) values[field] = key;
  else delete values[field];
  return { ...item, values };
}

export function AddToProjectGroup({
  grouping,
  value,
  name,
  onAdd,
  onCreateIssue,
}: {
  grouping?: GroupBy;
  value: string;
  name: string;
  onAdd?: AddProjectGroupItem;
  onCreateIssue?: (typeId: string | null) => void;
}) {
  if (!grouping) return null;
  const byType = grouping === "issue_type";
  if (byType ? !onCreateIssue || value === "other" : !onAdd) return null;
  const label = byType ? `Create issue in ${name}` : `Add work to ${name}`;
  return (
    <Button
      type="button"
      size="icon-sm"
      variant="ghost"
      aria-label={label}
      title={label}
      onClick={() => {
        if (grouping === "issue_type")
          onCreateIssue?.(value === "none" ? null : value);
        else onAdd?.(grouping, value);
      }}
    >
      <PlusIcon aria-hidden="true" />
    </Button>
  );
}

export function useProjectGroupMove(
  grouping: GroupBy | undefined,
  repo: Pick<Repository, "id" | "owner" | "name"> | undefined,
  refreshIssueType: (() => void) | undefined,
  editItem?: ChangeProjectItem,
) {
  const dragged = useRef<Row | null>(null);
  const [over, setOver] = useState<string>();
  const { csrf } = useProjectCommands();
  const mutation = useMutation(csrf, null);

  function movable(row: Row) {
    if (!grouping || row.restricted) return false;
    if (grouping !== "issue_type") return !!editItem;
    return (
      !!refreshIssueType &&
      !!(row.repository ?? repo) &&
      row.work?.subject.kind === "issue" &&
      row.issue_type_version != null &&
      !mutation.pending &&
      !mutation.conflict
    );
  }

  async function move(row: Row, key: string) {
    if (!grouping) return;
    if (grouping !== "issue_type") {
      editItem?.(row, (item) => applyProjectGroup(item, grouping, key));
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
        type_id: key === "none" ? null : key,
      },
      source,
    );
    if (saved) refreshIssueType?.();
  }

  function endDrag() {
    dragged.current = null;
    setOver(undefined);
  }

  function draggable(row: Row) {
    if (!movable(row)) return { draggable: false };
    return {
      draggable: true,
      onDragStart: (event: DragEvent<HTMLElement>) => {
        event.dataTransfer.effectAllowed = "move";
        event.dataTransfer.setData("text/plain", row.item.id);
        dragged.current = row;
      },
      onDragEnd: endDrag,
    };
  }

  function dropTarget(key: string) {
    function accepts(row: Row) {
      return (
        !!grouping &&
        movable(row) &&
        (grouping !== "issue_type" || key !== "other") &&
        rowGroup(row, grouping) !== key
      );
    }
    function allow(event: DragEvent<HTMLElement>) {
      if (dragged.current && accepts(dragged.current)) {
        event.preventDefault();
        event.dataTransfer.dropEffect = "move";
        setOver(key);
      }
    }
    return {
      onDragEnter: allow,
      onDragOver: allow,
      onDragLeave: (event: DragEvent<HTMLElement>) => {
        if (
          !(event.relatedTarget instanceof Node) ||
          !event.currentTarget.contains(event.relatedTarget)
        ) {
          setOver((current) => (current === key ? undefined : current));
        }
      },
      onDrop: (event: DragEvent<HTMLElement>) => {
        event.preventDefault();
        if (dragged.current && accepts(dragged.current)) {
          void move(dragged.current, key);
        }
        endDrag();
      },
    };
  }

  return {
    draggable,
    dropTarget,
    movable,
    over,
    error: mutation.error,
    reload:
      mutation.conflict && refreshIssueType
        ? () => {
            mutation.reset();
            refreshIssueType();
          }
        : undefined,
  };
}

export function ProjectGroupMoveFailure({
  error,
  reload,
}: {
  error?: string;
  reload?: () => void;
}) {
  return (
    <>
      {error && (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}
      {reload && (
        <Button variant="outline" size="sm" onClick={reload}>
          Reload project items
        </Button>
      )}
    </>
  );
}
