import { useRef, useState, type DragEvent } from "react";
import { useMutation } from "@/components/discussion/mutations";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { endpoint, type Repository } from "@/lib/api";
import { useProjectCommands } from "./commands";
import type { ItemPage, Row } from "./model";

function issueTypeGroup(row: Row) {
  return row.issue_type_group ?? "other";
}

export function issueTypeGroups(page: ItemPage) {
  const counts = page.issue_type_counts;
  if (!counts) return undefined;

  const types = (page.issue_types ?? []).map((type) => ({
    key: type.id,
    name: type.name,
  }));
  types.push({ key: "none", name: "No type" });
  if (counts.other) types.push({ key: "other", name: "Other work" });
  const groups = types.map(({ key, name }) => ({
    key,
    name,
    count: counts[key] ?? 0,
    rows: [] as Row[],
  }));
  const byKey = new Map(groups.map((group) => [group.key, group]));
  for (const row of page.items) byKey.get(issueTypeGroup(row))?.rows.push(row);
  return groups;
}

export function useProjectGroupMove(
  repo: Pick<Repository, "id" | "owner" | "name"> | undefined,
  refreshIssueType: (() => void) | undefined,
  changeStatus?: (row: Row, status: string) => void,
) {
  const dragged = useRef<Row | null>(null);
  const [over, setOver] = useState<string>();
  const { csrf } = useProjectCommands();
  const mutation = useMutation(csrf, null);

  function movable(row: Row) {
    if (row.restricted) return false;
    if (changeStatus) return true;
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
    if (changeStatus) {
      changeStatus(row, key);
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
        movable(row) &&
        (changeStatus
          ? row.item.status !== key
          : key !== "other" && issueTypeGroup(row) !== key)
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
