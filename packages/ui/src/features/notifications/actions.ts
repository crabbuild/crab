export type Address = { id: string; owner: string; name: string };
export const INBOX_CHANGED = "crab-inbox-changed";

export function refreshNotificationBadge() {
  window.dispatchEvent(new Event(INBOX_CHANGED));
}
export type NoticeState = { read: boolean; saved: boolean; done: boolean };
export type Action = { read: boolean } | { save: boolean } | { done: boolean };
export type Subject = {
  kind: "issue" | "pull" | "release" | "issue_transfer";
  number: number;
};
type Position = { id: string; created_at_ms: number };

export type Selection = {
  id: string;
  repository: Address;
  subject: Subject;
  updated: Position;
};

type Item = { id: string; repository_id: string; expected: Position };

export const bulkActions = {
  read: { label: "Mark as read", action: { read: true } },
  unread: { label: "Mark as unread", action: { read: false } },
  save: { label: "Save", action: { save: true } },
  unsave: { label: "Unsave", action: { save: false } },
  done: { label: "Mark as done", action: { done: true } },
  restore: { label: "Move to inbox", action: { done: false } },
  unsubscribe: { label: "Unsubscribe", action: null },
} satisfies Record<string, { label: string; action: Action | null }>;

export type BulkAction = keyof typeof bulkActions;
export type BulkIntent =
  | {
      kind: "triage";
      body: {
        request_id: string;
        issued_at_ms: number;
        items: Item[];
        action: Action;
      };
    }
  | {
      kind: "unsubscribe";
      body: { items: (Item & { subject: Subject })[] };
    };

export function isBulkAction(value: string): value is BulkAction {
  return Object.hasOwn(bulkActions, value);
}

export function notificationIntent(
  action: BulkAction,
  selection: Selection[],
): BulkIntent {
  const items = selection.map((row) => ({
    id: row.id,
    repository_id: row.repository.id,
    expected: row.updated,
  }));
  const desired = bulkActions[action].action;
  if (desired)
    return {
      kind: "triage",
      body: {
        request_id: crypto.randomUUID(),
        issued_at_ms: Date.now(),
        items,
        action: desired,
      },
    };
  return {
    kind: "unsubscribe",
    body: {
      items: items.map((item, index) => ({
        ...item,
        subject: selection[index].subject,
      })),
    },
  };
}

export type UnsubscribeStatus = "unsubscribed" | "changed" | "unavailable";
