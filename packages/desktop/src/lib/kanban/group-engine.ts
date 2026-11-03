import type { KanbanCardInfo, KanbanCardPriority, KanbanColumnInfo } from "../../ipc";

export type GroupingMode = "default" | "priority" | "label" | "assignee";

export interface VirtualColumn {
  id: string;
  name: string;
  cards: KanbanCardInfo[];
}

const PRIORITY_ORDER: KanbanCardPriority[] = [
  "critical",
  "high",
  "medium",
  "low",
  "none",
];

const PRIORITY_LABELS: Record<KanbanCardPriority, string> = {
  critical: "Critical",
  high: "High",
  medium: "Medium",
  low: "Low",
  none: "None",
};

/**
 * Groups cards into virtual columns based on the selected grouping mode.
 * When mode is "default", returns the original column structure.
 */
export function groupCards(
  cards: KanbanCardInfo[],
  columns: KanbanColumnInfo[],
  mode: GroupingMode,
): VirtualColumn[] {
  switch (mode) {
    case "default":
      return groupByDefault(cards, columns);
    case "priority":
      return groupByPriority(cards);
    case "assignee":
      return groupByAssignee(cards);
    case "label":
      return groupByLabel(cards);
  }
}

/**
 * When a card is dragged between groups, computes the field update
 * needed to match the target group.
 */
export function computeGroupFieldUpdate(
  _card: KanbanCardInfo,
  targetGroupId: string,
  mode: GroupingMode,
): Partial<KanbanCardInfo> {
  switch (mode) {
    case "default":
      return { columnId: targetGroupId };
    case "priority":
      return { priority: targetGroupId as KanbanCardPriority };
    case "assignee": {
      if (targetGroupId === "__unassigned__") {
        return { assignee: undefined };
      }
      return { assignee: targetGroupId };
    }
    case "label": {
      if (targetGroupId === "__unlabeled__") {
        return { labels: [] };
      }
      const card = _card;
      const currentLabels = card.labels ?? [];
      if (currentLabels.length === 0) {
        return { labels: [targetGroupId] };
      }
      // Replace the first label with the target group label
      return { labels: [targetGroupId, ...currentLabels.slice(1)] };
    }
  }
}

function groupByDefault(
  cards: KanbanCardInfo[],
  columns: KanbanColumnInfo[],
): VirtualColumn[] {
  const columnMap = new Map<string, KanbanCardInfo[]>();
  for (const col of columns) {
    columnMap.set(col.id, []);
  }
  for (const card of cards) {
    const bucket = columnMap.get(card.columnId);
    if (bucket) {
      bucket.push(card);
    }
  }
  return columns.map((col) => ({
    id: col.id,
    name: col.name,
    cards: columnMap.get(col.id) ?? [],
  }));
}

function groupByPriority(cards: KanbanCardInfo[]): VirtualColumn[] {
  const groups = new Map<KanbanCardPriority, KanbanCardInfo[]>();
  for (const p of PRIORITY_ORDER) {
    groups.set(p, []);
  }
  for (const card of cards) {
    const bucket = groups.get(card.priority);
    if (bucket) {
      bucket.push(card);
    }
  }
  return PRIORITY_ORDER.map((p) => ({
    id: p,
    name: PRIORITY_LABELS[p],
    cards: groups.get(p) ?? [],
  }));
}

function groupByAssignee(cards: KanbanCardInfo[]): VirtualColumn[] {
  const groups = new Map<string, KanbanCardInfo[]>();
  const assignees = new Set<string>();

  for (const card of cards) {
    const key = card.assignee ?? "__unassigned__";
    if (card.assignee) {
      assignees.add(card.assignee);
    }
    if (!groups.has(key)) {
      groups.set(key, []);
    }
    groups.get(key)!.push(card);
  }

  // Sort assignees alphabetically, "Unassigned" last
  const sorted = [...assignees].sort((a, b) =>
    a.localeCompare(b, undefined, { sensitivity: "base" }),
  );

  const result: VirtualColumn[] = sorted.map((name) => ({
    id: name,
    name,
    cards: groups.get(name) ?? [],
  }));

  // Add "Unassigned" column last (always present)
  result.push({
    id: "__unassigned__",
    name: "Unassigned",
    cards: groups.get("__unassigned__") ?? [],
  });

  return result;
}

function groupByLabel(cards: KanbanCardInfo[]): VirtualColumn[] {
  const groups = new Map<string, KanbanCardInfo[]>();
  const labels = new Set<string>();

  for (const card of cards) {
    const firstLabel = card.labels.length > 0 ? card.labels[0] : null;
    const key = firstLabel ?? "__unlabeled__";
    if (firstLabel) {
      labels.add(firstLabel);
    }
    if (!groups.has(key)) {
      groups.set(key, []);
    }
    groups.get(key)!.push(card);
  }

  // Sort labels alphabetically, "Unlabeled" last
  const sorted = [...labels].sort((a, b) =>
    a.localeCompare(b, undefined, { sensitivity: "base" }),
  );

  const result: VirtualColumn[] = sorted.map((label) => ({
    id: label,
    name: label,
    cards: groups.get(label) ?? [],
  }));

  // Add "Unlabeled" column last (always present)
  result.push({
    id: "__unlabeled__",
    name: "Unlabeled",
    cards: groups.get("__unlabeled__") ?? [],
  });

  return result;
}
