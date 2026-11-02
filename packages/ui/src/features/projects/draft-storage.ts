import type {
  Definition,
  Item,
  Project,
  ProjectTarget,
  Row,
  Work,
} from "./model";
import type { Properties } from "./workspace/model";

export type DraftData =
  | {
      kind: "create";
      name: string;
      template: string;
      sourceVersion: number | null;
    }
  | {
      kind: "settings";
      definition: Definition;
      statuses: string;
      archived: boolean;
      version: number;
    }
  | { kind: "properties"; properties: Properties; version: number }
  | {
      kind: "item";
      item: Item;
      definition: Definition;
      version: number;
      mode: "issue" | "existing" | "draft";
      parent: string;
      repository: string;
      work: Work | null;
      existing: boolean;
      restricted: boolean;
    };
export interface DraftScope {
  target: ProjectTarget;
  project: string | null;
  item?: string;
}
export interface DraftCheckpoint {
  key: string;
  content: string;
}
export interface DraftRecord {
  format: 1;
  account: string;
  project: string | null;
  item: string;
  data: DraftData;
}
export const draftEvent = "crab-project-drafts-changed";
export function itemDraft(
  project: Project,
  row: Row,
  repository: string,
): Extract<DraftData, { kind: "item" }> {
  return {
    kind: "item",
    item: row.item,
    definition: project.definition,
    version: row.version,
    mode: row.item.subject ? "existing" : "draft",
    parent: "",
    repository,
    work: row.work,
    existing: true,
    restricted: !!row.restricted,
  };
}
function prefix(account: string, scope: DraftScope) {
  const target =
    "repo" in scope.target
      ? `repository:${scope.target.repo.id}`
      : `organization:${scope.target.organization}`;
  return `crab.projects.draft.v1:${encodeURIComponent(account)}:${target}:${scope.project ?? "new"}:`;
}
export function draftKey(
  account: string,
  scope: DraftScope,
  kind: DraftData["kind"],
) {
  return `${prefix(account, scope)}${kind}:${scope.item ?? "new"}`;
}
function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
const string = (value: unknown): value is string => typeof value === "string";
const strings = (value: unknown): value is string[] =>
  Array.isArray(value) && value.every(string);
const optionalString = (value: unknown) => value === null || string(value);
const revision = (value: unknown): value is number =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
function subject(value: unknown) {
  return (
    object(value) &&
    (value.kind === "issue" || value.kind === "pull") &&
    revision(value.number) &&
    value.number > 0
  );
}
function definition(value: unknown): boolean {
  return (
    object(value) &&
    string(value.name) &&
    string(value.description) &&
    string(value.readme) &&
    strings(value.statuses) &&
    typeof value.is_template === "boolean" &&
    Array.isArray(value.fields) &&
    value.fields.every(
      (field) =>
        object(field) &&
        string(field.id) &&
        string(field.name) &&
        object(field.definition) &&
        (["text", "number", "date"].includes(String(field.definition.kind)) ||
          (field.definition.kind === "select" &&
            strings(field.definition.options))),
    ) &&
    Array.isArray(value.views) &&
    value.views.every(
      (view) =>
        object(view) &&
        string(view.id) &&
        string(view.name) &&
        ["table", "board", "roadmap"].includes(String(view.layout)) &&
        string(view.query) &&
        optionalString(view.status) &&
        typeof view.hide_closed === "boolean",
    )
  );
}
function item(value: unknown): boolean {
  return (
    object(value) &&
    string(value.id) &&
    (value.subject === null || subject(value.subject)) &&
    string(value.title) &&
    string(value.body) &&
    string(value.status) &&
    revision(value.priority) &&
    optionalString(value.start) &&
    optionalString(value.end) &&
    object(value.values) &&
    Object.values(value.values).every(string)
  );
}
function valid(value: unknown): value is DraftData {
  if (!object(value)) return false;
  switch (value.kind) {
    case "create":
      return (
        string(value.name) &&
        string(value.template) &&
        (value.sourceVersion === null || revision(value.sourceVersion))
      );
    case "settings":
      return (
        definition(value.definition) &&
        string(value.statuses) &&
        typeof value.archived === "boolean" &&
        revision(value.version)
      );
    case "properties":
      return (
        revision(value.version) &&
        object(value.properties) &&
        ["planned", "in_progress", "paused", "completed", "cancelled"].includes(
          String(value.properties.status),
        ) &&
        ["on_track", "at_risk", "off_track"].includes(
          String(value.properties.health),
        ) &&
        optionalString(value.properties.lead) &&
        optionalString(value.properties.start) &&
        optionalString(value.properties.target)
      );
    case "item":
      return (
        item(value.item) &&
        definition(value.definition) &&
        revision(value.version) &&
        ["issue", "existing", "draft"].includes(String(value.mode)) &&
        string(value.parent) &&
        string(value.repository) &&
        typeof value.existing === "boolean" &&
        typeof value.restricted === "boolean" &&
        (value.work === null ||
          (object(value.work) &&
            subject(value.work.subject) &&
            string(value.work.title) &&
            ["open", "closed", "merged"].includes(String(value.work.state))))
      );
    default:
      return false;
  }
}
export function readDraft(
  account: string,
  scope: DraftScope,
  kind: DraftData["kind"],
): DraftData | null {
  const raw = sessionStorage.getItem(draftKey(account, scope, kind));
  if (raw === null) return null;
  const value: unknown = JSON.parse(raw);
  if (
    !object(value) ||
    value.format !== 1 ||
    value.account !== account ||
    value.project !== scope.project ||
    value.item !== (scope.item ?? "new") ||
    !valid(value.data) ||
    value.data.kind !== kind
  )
    throw new Error(
      "The saved draft could not be read. It has been retained; copy or export it before discarding it.",
    );
  if (
    value.data.kind === "item" &&
    scope.item &&
    scope.item !== "new" &&
    value.data.item.id !== scope.item
  )
    throw new Error(
      "The saved draft belongs to a different item. Its recovery record has been retained.",
    );
  return value.data;
}
export function checkpointDraft(
  account: string,
  scope: DraftScope,
  data: DraftData,
): DraftCheckpoint {
  const key = draftKey(account, scope, data.kind);
  // Captured revisions and every form field are saved together. Restoring only
  // text against a newer revision could silently replace a teammate's edit.
  const content = draftContent(account, scope, data);
  sessionStorage.setItem(key, content);
  window.dispatchEvent(new Event(draftEvent));
  return { key, content };
}
export function draftContent(
  account: string,
  scope: DraftScope,
  data: DraftData,
) {
  return JSON.stringify({
    format: 1,
    account,
    project: scope.project,
    item: scope.item ?? "new",
    data,
  } satisfies DraftRecord);
}
export function clearDraft(checkpoint: DraftCheckpoint) {
  // An acknowledgement may arrive after route changes. Clear only the exact
  // draft captured by that submission, never a replacement local draft.
  if (sessionStorage.getItem(checkpoint.key) !== checkpoint.content) return;
  sessionStorage.removeItem(checkpoint.key);
  window.dispatchEvent(
    new CustomEvent(draftEvent, { detail: { removed: true, ...checkpoint } }),
  );
}
export function listDrafts(account: string, scope: DraftScope) {
  const keys = Array.from({ length: sessionStorage.length }, (_, index) =>
    sessionStorage.key(index),
  ).filter(
    (key): key is string =>
      key !== null && key.startsWith(prefix(account, scope)),
  );
  return keys.map((key) => {
    const rest = key.slice(prefix(account, scope).length);
    const [kind, item] = rest.split(":");
    if (!["create", "settings", "properties", "item"].includes(kind) || !item)
      throw new Error(
        "A saved project draft has an unsupported format. Its storage record has been retained.",
      );
    return { kind: kind as DraftData["kind"], item, key };
  });
}
