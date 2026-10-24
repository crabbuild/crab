import type { Repository, RepositoryAssignee } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
export type Subject = { kind: "issue" | "pull"; number: number };
export interface Work {
  subject: Subject;
  title: string;
  state: "open" | "closed" | "merged";
}
export type Layout = "table" | "board" | "roadmap";
export interface View {
  id: string;
  name: string;
  layout: Layout;
  query: string;
  status: string | null;
  hide_closed: boolean;
}
export interface Field {
  id: string;
  name: string;
  definition:
    | { kind: "text" | "number" | "date" }
    | { kind: "select"; options: string[] };
}
export interface Definition {
  name: string;
  description: string;
  readme: string;
  statuses: string[];
  fields: Field[];
  views: View[];
  is_template: boolean;
}
export interface Project {
  id: string;
  definition: Definition;
  version: number;
  archived: boolean;
  created_at: number;
  updated_at: number;
}
export interface Item {
  id: string;
  subject: Subject | null;
  title: string;
  body: string;
  status: string;
  priority: number;
  start: string | null;
  end: string | null;
  values: Record<string, string>;
}
export interface Row {
  item: Item;
  work: Work | null;
  assignees: RepositoryAssignee[];
  parent: Work | null;
  progress: { total: number; closed: number } | null;
  links: Work[];
  more_links: boolean;
}
export interface ItemPage {
  project: Project;
  revision: number;
  items: Row[];
  next: string | null;
  counts: Record<string, number>;
  total: number;
}
export interface Listing {
  items: Project[];
  next: string | null;
  can_manage: boolean;
}
export interface Template {
  id: string;
  description: string;
  definition: Definition;
}
export type Command =
  | { kind: "sub_issue"; parent: number; title: string; body: string }
  | { kind: "create"; id: string; name: string; template: string }
  | { kind: "copy"; id: string; source: string; version: number; name: string }
  | {
      kind: "edit";
      project: string;
      version: number;
      definition: Definition;
      archived: boolean;
    }
  | { kind: "item"; project: string; version: number; item: Item }
  | {
      kind: "create_issue";
      project: string;
      version: number;
      item: Item;
      parent: number | null;
    }
  | { kind: "remove"; project: string; version: number; item: string }
  | { kind: "parent"; child: number; parent: number | null; version: number };
export type Outcome =
  | { outcome: "applied"; project: string | null; version: number }
  | { outcome: "cancelled" }
  | { outcome: "rejected"; reason: string };
export function projectHref(repo: Repository, project?: string) {
  return repoHref(repo, { view: "projects", project });
}
export function workHref(repo: Repository, subject: Subject) {
  return repoHref(repo, {
    view: subject.kind === "issue" ? "issues" : "pulls",
    issue: subject.kind === "issue" ? String(subject.number) : undefined,
    pull: subject.kind === "pull" ? String(subject.number) : undefined,
  });
}
export function blankItem(status: string): Item {
  return {
    id: crypto.randomUUID(),
    subject: null,
    title: "",
    body: "",
    status,
    priority: 0,
    start: null,
    end: null,
    values: {},
  };
}
export const priorities = ["No priority", "Urgent", "High", "Medium", "Low"];
