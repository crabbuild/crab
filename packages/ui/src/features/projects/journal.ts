import type { Command, ProjectTarget } from "./model";
import type { WorkspaceCommand } from "./workspace/model";
import type { DraftCheckpoint } from "./draft-storage";

export interface ProjectIntent {
  account: string;
  target: ProjectTarget;
  body: { request_id: string; command: Command | WorkspaceCommand };
  draft?: DraftCheckpoint;
}

function key(account: string) {
  return `crab.projects.submission.v1:${account}`;
}

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function uuid(value: unknown): value is string {
  return (
    typeof value === "string" &&
    /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(value)
  );
}
function target(value: unknown): value is ProjectTarget {
  if (!object(value)) return false;
  if ("organization" in value)
    return uuid(value.organization) && !("repo" in value);
  return (
    object(value.repo) &&
    uuid(value.repo.id) &&
    typeof value.repo.owner === "string" &&
    value.repo.owner.length > 0 &&
    typeof value.repo.name === "string" &&
    value.repo.name.length > 0 &&
    typeof value.repo.archived === "boolean"
  );
}

export function readProjectIntent(account: string): ProjectIntent | null {
  let raw: string | null;
  try {
    raw = sessionStorage.getItem(key(account));
  } catch (cause) {
    throw new Error(
      "Browser session storage is unavailable. Allow storage for this site, then retry recovery.",
      { cause },
    );
  }
  if (raw === null) return null;
  let saved: unknown;
  try {
    saved = JSON.parse(raw);
  } catch (cause) {
    throw new Error(
      "The saved submission could not be decoded. Its recovery record has been retained.",
      { cause },
    );
  }
  if (!object(saved) || saved.format !== 1 || !object(saved.intent))
    throw new Error(
      "The saved project submission could not be read. Its recovery record has been retained.",
    );
  const intent = saved.intent;
  if (
    intent.account !== account ||
    !target(intent.target) ||
    !object(intent.body) ||
    !uuid(intent.body.request_id) ||
    !object(intent.body.command) ||
    (intent.draft !== undefined &&
      (!object(intent.draft) ||
        typeof intent.draft.key !== "string" ||
        !intent.draft.key.startsWith(
          `crab.projects.draft.v1:${encodeURIComponent(account)}:`,
        ) ||
        typeof intent.draft.content !== "string")) ||
    ![
      "create",
      "copy",
      "edit",
      "planning",
      "repository",
      "item",
      "remove",
      "parent",
      "sub_issue",
      "create_issue",
    ].includes(String(intent.body.command.kind))
  )
    throw new Error(
      "The saved project submission does not match this account or project format. Its recovery record has been retained.",
    );
  // The journal checks identity and routing. The native command owner validates
  // the unchanged payload before any effect; browser data grants no authority.
  return intent as unknown as ProjectIntent;
}

export function saveProjectIntent(intent: ProjectIntent) {
  // A tab session survives reload without sharing another tab's pending writer.
  // Persist before sending: a quota/security error must prevent a new write.
  const previous = readProjectIntent(intent.account);
  if (previous && JSON.stringify(previous) !== JSON.stringify(intent))
    throw new Error(
      "Another saved project submission needs recovery before new changes can be sent.",
    );
  try {
    sessionStorage.setItem(
      key(intent.account),
      JSON.stringify({ format: 1, intent }),
    );
  } catch (cause) {
    throw new Error(
      "The browser could not save the recovery record. Free site storage or allow storage, then retry. No new changes were sent.",
      { cause },
    );
  }
}

export function removeProjectIntent(intent: ProjectIntent) {
  const saved = readProjectIntent(intent.account);
  if (!saved) return;
  if (
    saved.body.request_id !== intent.body.request_id ||
    JSON.stringify(saved) !== JSON.stringify(intent)
  )
    throw new Error(
      "The saved submission changed. Reload to recover it before making more changes.",
    );
  try {
    sessionStorage.removeItem(key(intent.account));
  } catch (cause) {
    throw new Error(
      "The result was received, but its recovery record could not be cleared. Retry this submission to confirm the saved result.",
      { cause },
    );
  }
}
