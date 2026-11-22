import type { ActorSummary } from "@/lib/contracts";
import { endpoint, useRequest, type Repository } from "@/lib/api";
import { repoHref } from "@/lib/repository-route";
import { useEffect, useState } from "react";

export type Trigger = {
  kind: "command" | "mention" | "reference";
  query: string;
  repository?: Pick<Repository, "owner" | "name">;
  start: number;
  end: number;
};

export function triggerAt(
  value: string,
  start: number,
  end: number,
): Trigger | null {
  if (start !== end) return null;
  const lineStart = start === 0 ? 0 : value.lastIndexOf("\n", start - 1) + 1;
  const line = value.slice(lineStart, start);
  const command = /^ {0,3}\/(\w[\w-]{0,39}|)$/.exec(line);
  const mention =
    /(?:^|[\s([{])@([a-z\d-]{1,64}\/[a-z\d-]{0,64}|[a-z\d-]{0,39})$/i.exec(
      line,
    );
  const reference =
    /(?:^|[\s([{])(?:([A-Za-z\d_.-]{1,100})\/([A-Za-z\d_.-]{1,100}))?#((?:[\p{L}\p{N}_-][\p{L}\p{N}_ -]{0,79})?)$/u.exec(
      line,
    );
  const [, owner, name, referenceQuery] = reference ?? [];
  const query = command?.[1] ?? mention?.[1] ?? referenceQuery;
  if (
    query === undefined ||
    (referenceQuery !== undefined && /^\d+ /.test(referenceQuery)) ||
    [owner, name].some((part) => part === "." || part === "..") ||
    /^[\p{L}\p{N}_/-]/u.test(value.slice(end))
  ) {
    return null;
  }
  const repository = owner && name ? { owner, name } : undefined;
  // Commands insert blocks. A trigger inside a code span or fence belongs to
  // the example being written, so it must not intercept Enter or Tab.
  const fence = /^ {0,3}(`{3,}|~{3,})([^\r\n]*)/gm;
  let openFence: string | null = null;
  for (const match of value.slice(0, lineStart).matchAll(fence)) {
    if (!openFence) openFence = match[1];
    else if (
      match[1][0] === openFence[0] &&
      match[1].length >= openFence.length &&
      !match[2].trim()
    )
      openFence = null;
  }
  if (openFence || /(^|[^\\])`+[^`]*$/.test(line.replace(/(`+)(.*?)\1/g, "")))
    return null;
  return {
    kind: command ? "command" : mention ? "mention" : "reference",
    query,
    repository,
    start:
      start -
      query.length -
      (repository ? repository.owner.length + repository.name.length + 2 : 1),
    end,
  };
}

type Snippet = {
  id: string;
  label: string;
  description?: string;
  text: string;
  selection?: string;
};

const alerts = ["NOTE", "TIP", "IMPORTANT", "WARNING", "CAUTION"];

const commands: Snippet[] = [
  {
    id: "alert",
    label: "Alert",
    description: "Call attention to important information",
    text: "",
  },
  {
    id: "code",
    label: "Code block",
    description: "Insert a fenced code block",
    text: "",
  },
  {
    id: "details",
    label: "Details",
    description: "Add a collapsible section",
    text: "<details>\n<summary>Summary</summary>\n\nDetails go here.\n\n</details>",
    selection: "Summary",
  },
  {
    id: "table",
    label: "Table",
    description: "Insert a table with two columns",
    text: "| Column 1 | Column 2 |\n| --- | --- |\n| Value | Value |",
    selection: "Column 1",
  },
  {
    id: "task-list",
    label: "Task list",
    description: "Create a checklist",
    text: "- [ ] Task",
    selection: "Task",
  },
  {
    id: "divider",
    label: "Divider",
    description: "Separate sections with a horizontal rule",
    text: "---",
  },
  { id: "emoji", label: "Emoji", description: "Insert an emoji", text: "" },
  {
    id: "saved-replies",
    label: "Saved replies",
    description: "Insert a reply from your account",
    text: "",
  },
];

const commandOptions = {
  alert: alerts.map((alert) => ({
    id: alert,
    label: alert[0] + alert.slice(1).toLowerCase(),
    text: `> [!${alert}]\n> Add context here.`,
    selection: "Add context here.",
  })),
  code: [
    "Plain text",
    "Bash",
    "JSON",
    "JavaScript",
    "Markdown",
    "Python",
    "Rust",
    "SQL",
    "TypeScript",
    "YAML",
  ].map((language) => ({
    id: language,
    label: language,
    text: `\`\`\`${language === "Plain text" ? "" : language.toLowerCase()}\nYour code here\n\`\`\``,
    selection: "Your code here",
  })),
  emoji: [
    { id: "thumbs-up", label: "Thumbs up", text: "👍" },
    { id: "tada", label: "Celebrate", text: "🎉" },
    { id: "eyes", label: "Eyes", text: "👀" },
    { id: "heart", label: "Heart", text: "❤️" },
    { id: "rocket", label: "Rocket", text: "🚀" },
    { id: "check", label: "Check mark", text: "✅" },
  ],
  "saved-replies": [],
} satisfies Record<string, Snippet[]>;

export type CommandMenu = keyof typeof commandOptions;

export type DiscussionReference = {
  subject: { kind: "issue" | "pull"; number: number };
  title: string;
  state: "open" | "closed" | "merged";
};

export type ReferenceSearch = {
  repository_id: string;
  items: DiscussionReference[];
};

export function stableReferenceHref(
  repository: string,
  { kind, number }: DiscussionReference["subject"],
) {
  return `/api/discussion-references/${encodeURIComponent(repository)}/${kind}/${number}`;
}

export type Suggestion = Snippet & {
  kind: "command" | "mention" | "team" | DiscussionReference["subject"]["kind"];
  state?: DiscussionReference["state"];
  cursor?: number;
};

type Scope = Pick<Repository, "id" | "owner" | "name">;
type TeamOption = Pick<ActorSummary, "id" | "handle" | "name"> & {
  organization: string;
};

export function useEditorSuggestions(
  repo: Scope | null,
  trigger: Trigger | null,
  command: CommandMenu | null,
) {
  const kind = trigger?.kind;
  const query = trigger?.query ?? "";
  const team = kind === "mention" && query.includes("/");
  const cross = kind === "reference" ? trigger?.repository : undefined;
  const target = cross ?? repo;
  const url =
    (kind === "mention" || kind === "reference") && target
      ? endpoint(
          target,
          `discussion-search/${team ? "team-mentions" : `${kind}s`}`,
          {
            repository_id: cross ? undefined : repo?.id,
            search: query,
          },
        )
      : null;
  const saved = kind === "command" && command === "saved-replies";
  const replies = useRequest<{
    items: { id: string; title: string; body: string; version: number }[];
  }>(saved ? "/api/profile/saved-replies" : null);
  const [requested, setRequested] = useState<string | null>(null);
  useEffect(() => {
    if (!url) return;
    const timer = setTimeout(() => setRequested(url), 150);
    return () => clearTimeout(timer);
  }, [url]);
  const resource = useRequest<
    ReferenceSearch | { items: ActorSummary[] | TeamOption[] }
  >(url === requested ? url : null);
  let items: Suggestion[] = [];
  if (saved) {
    items = (replies.data?.items ?? []).map(({ id, title, body }) => {
      const marker = body.indexOf("%cursor%");
      return {
        id,
        kind: "command",
        label: title,
        text: body.replaceAll("%cursor%", ""),
        cursor: marker === -1 ? undefined : marker,
      };
    });
  } else if (kind === "command") {
    const term = query.toLowerCase();
    items = (
      command
        ? commandOptions[command]
        : commands.filter(
            (item) =>
              item.id.includes(term) || item.label.toLowerCase().includes(term),
          )
    ).map((item) => ({ ...item, kind: "command" }));
  } else if (team && resource.data) {
    items = (resource.data.items as TeamOption[]).map(
      ({ id, organization, handle, name }) => ({
        kind: "team",
        id,
        label: `@${handle}`,
        description: name,
        text: `[@${handle}](/-/organizations/${organization}?tab=teams&team=${id})`,
      }),
    );
  } else if (kind === "mention" && resource.data) {
    items = (resource.data.items as ActorSummary[]).map(
      ({ id, handle, name }) => ({
        kind: "mention",
        id,
        label: `@${handle}`,
        description: name,
        text: `[@${handle}](/-/users/${encodeURIComponent(id)})`,
      }),
    );
  } else if (kind === "reference" && target && resource.data) {
    const result = resource.data as ReferenceSearch;
    items = result.items.flatMap(({ subject, title, state }) => {
      const number = String(subject.number);
      const issue = subject.kind === "issue";
      if (cross && !result.repository_id) return [];
      const href = cross
        ? stableReferenceHref(result.repository_id, subject)
        : repoHref(
            target,
            issue
              ? { view: "issues", issue: number }
              : { view: "pulls", pull: number },
          );
      const label = cross
        ? `${target.owner}/${target.name}#${number}`
        : `#${number}`;
      return [
        {
          kind: subject.kind,
          state,
          id: `${subject.kind}-${number}`,
          label: title,
          description: `${label} · ${issue ? "Issue" : "Pull request"} · ${state}`,
          text: `[${label}](${href.replaceAll("(", "%28").replaceAll(")", "%29")})`,
        },
      ];
    });
  }
  return {
    items,
    loading: saved
      ? replies.loading
      : !!url && (requested !== url || resource.loading),
    error: saved ? replies.error : resource.error,
  };
}

export function hasCommandOptions(id: string): id is CommandMenu {
  return Object.hasOwn(commandOptions, id);
}
