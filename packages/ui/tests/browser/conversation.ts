import { Buffer } from "node:buffer";
import type { Route } from "@playwright/test";

type Kind = "comment" | "review" | "activity";
interface Entry {
  kind: Kind;
  value: object;
}
function number(value: object, field: "number" | "created_at") {
  const candidate =
    field in value
      ? (value as Record<typeof field, unknown>)[field]
      : undefined;
  if (
    typeof candidate !== "number" ||
    !Number.isSafeInteger(candidate) ||
    candidate < 0
  )
    throw new Error("Conversation fixture lacks " + field);
  return candidate;
}
function key(entry: Entry) {
  return entry.kind + ":" + number(entry.value, "number");
}
function kind(value: unknown) {
  return typeof value === "object" && value !== null && "kind" in value
    ? value.kind
    : undefined;
}
function projected(value: object) {
  const target = "target" in value ? kind(value.target) : undefined;
  const action = "action" in value ? kind(value.action) : undefined;
  return (
    target !== "issue_comment" &&
    target !== "pull_comment" &&
    target !== "pull_review" &&
    !(action === "created" && (target === "issue" || target === "pull"))
  );
}
export function conversationPage(
  route: Route,
  {
    comments = [],
    reviews = [],
    activity = [],
    head = null,
  }: {
    comments?: readonly object[];
    reviews?: readonly object[];
    activity?: readonly object[];
    head?: string | null;
  } = {},
) {
  const sourceOrder = { comment: 0, review: 1, activity: 2 };
  const entries: Entry[] = [
    ...comments.map((value) => ({ kind: "comment" as const, value })),
    ...reviews.map((value) => ({ kind: "review" as const, value })),
    ...activity
      .filter(projected)
      .map((value) => ({ kind: "activity" as const, value })),
  ];
  entries.sort(
    (a, b) =>
      number(a.value, "created_at") - number(b.value, "created_at") ||
      sourceOrder[a.kind] - sourceOrder[b.kind] ||
      number(a.value, "number") - number(b.value, "number"),
  );
  const query = new URL(route.request().url()).searchParams;
  const limit = Number(query.get("limit") ?? 30);
  const focus = query.get("focus"),
    before = query.get("before"),
    after = query.get("after");
  const cursor = before ?? after;
  const selected =
    focus ?? (cursor ? Buffer.from(cursor, "base64url").toString() : undefined);
  const index = selected
    ? entries.findIndex((entry) => key(entry) === selected)
    : -1;
  if (selected && index < 0)
    return route.fulfill({
      status: focus ? 404 : 409,
      json: {
        error: {
          message: focus
            ? "Contribution not found"
            : "Conversation cursor changed",
        },
      },
    });
  const start = after
    ? index + 1
    : Math.max(
        0,
        (focus ? index + 1 : before ? index : entries.length) - limit,
      );
  const end = after
    ? Math.min(entries.length, start + limit)
    : focus
      ? index + 1
      : before
        ? index
        : entries.length;
  const encode = (entry: Entry) =>
    Buffer.from(key(entry)).toString("base64url");
  return route.fulfill({
    json: {
      items: entries.slice(start, end),
      start: start < end ? encode(entries[start]) : null,
      end: start < end ? encode(entries[end - 1]) : null,
      has_earlier: start > 0 && start < end,
      has_later: end < entries.length && start < end,
      head_oid: head,
    },
  });
}
