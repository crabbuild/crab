import type { RootContent } from "mdast";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import { unified } from "unified";
import { endpoint, request, type Repository } from "@/lib/api";
import {
  stableReferenceHref,
  type ReferenceSearch,
} from "./editor-suggestions";

type Scope = Pick<Repository, "id" | "owner" | "name">;
type SourceText = {
  value: string;
  position?: { start: { offset?: number }; end: { offset?: number } };
};

export const maxReferenceLookups = 12;

const shorthand =
  /(?<![\p{L}\p{N}_.@/\\-])(?:([A-Za-z\d_.-]{1,100})\/([A-Za-z\d_.-]{1,100}))?#(\d{1,16})(?![\p{L}\p{N}_/-])/gu;
// Markdown leaves math delimiters and inline HTML contents in prose nodes.
// Pair the delimiters by source offset even when formatting splits a text node.
function literalRanges(source: string) {
  const ranges: { start: number; end: number }[] = [];
  const tags = new Map<string, number[]>();
  let inline: number | undefined;
  let display: number | undefined;
  for (const match of source.matchAll(
    /(?<!\\)\$+|[\r\n]|(?<!\\)<(\/?)([A-Za-z][\w:-]*)\b[^<>\r\n]*>/g,
  )) {
    const [token, closing, tag] = match;
    const end = match.index + token.length;
    if (tag) {
      const name = tag.toLowerCase();
      const starts = tags.get(name) ?? [];
      if (closing) {
        const start = starts.pop();
        if (start !== undefined) ranges.push({ start, end });
      } else if (!token.endsWith("/>")) {
        starts.push(match.index);
        tags.set(name, starts);
      }
    } else if (token === "$$") {
      if (display === undefined) display = match.index;
      else {
        ranges.push({ start: display, end });
        display = undefined;
      }
    } else if (token === "$") {
      if (
        inline !== undefined &&
        !/\s/.test(source[match.index - 1] ?? "") &&
        !/\d/.test(source[end] ?? "")
      ) {
        ranges.push({ start: inline, end });
        inline = undefined;
      } else if (
        inline === undefined &&
        source[end] &&
        !/\s/.test(source[end])
      ) {
        inline = match.index;
      }
    } else if (token === "\r" || token === "\n") inline = undefined;
  }
  const merged: typeof ranges = [];
  for (const range of ranges.sort((left, right) => left.start - right.start)) {
    const last = merged.at(-1);
    if (last && last.end >= range.start)
      last.end = Math.max(last.end, range.end);
    else merged.push(range);
  }
  return merged;
}

export function referenceMatcher(
  source: string,
  repository: Scope | null,
  crossOnly = false,
) {
  const literal = source.includes("#") ? literalRanges(source) : [];

  return function* ({ value, position }: SourceText) {
    const start = position?.start.offset;
    const end = position?.end.offset;
    // Markdown text values have escapes and entities decoded. Require the
    // literal source spelling before creating or rewriting a reference.
    if (
      start === undefined ||
      end === undefined ||
      !value.includes("#") ||
      source.slice(start, end) !== value
    )
      return;

    for (const match of value.matchAll(shorthand)) {
      const [label, owner, name, query] = match;
      const matchEnd = match.index + label.length;
      const number = Number(query);
      if (
        number < 1 ||
        !Number.isSafeInteger(number) ||
        [owner, name].some((part) => part === "." || part === "..") ||
        (crossOnly && !owner)
      )
        continue;
      let left = 0;
      let right = literal.length;
      while (left < right) {
        const middle = (left + right) >>> 1;
        if (literal[middle].end <= start + match.index) left = middle + 1;
        else right = middle;
      }
      if (literal[left] && literal[left].start < start + matchEnd) continue;

      const target = owner && name ? { owner, name } : repository;
      if (!target) continue;
      yield {
        label,
        number,
        start: match.index,
        end: matchEnd,
        url: endpoint(target, "discussion-search/references", {
          repository_id: owner ? undefined : repository?.id,
          search: String(number),
          exact: "true",
        }),
      };
    }
  };
}

export function uniqueReferenceHref(
  result: ReferenceSearch | undefined,
  number: number,
): string | undefined {
  if (!result?.repository_id || result.items.length !== 1) return;
  const { subject } = result.items[0];
  if (subject.number !== number) return;
  return stableReferenceHref(result.repository_id, subject);
}

const parser = unified().use(remarkParse).use(remarkGfm).freeze();
const bytes = new TextEncoder();
const nonProse = new Set([
  "link",
  "linkReference",
  "inlineCode",
  "code",
  "html",
]);

export async function bindCrossReferences(
  body: string,
  repo: Scope,
): Promise<string> {
  if (!body.includes("#") || !body.includes("/")) return body;
  let size = bytes.encode(body).byteLength;
  if (size > 65_536) return body;
  const matches = referenceMatcher(body, repo, true);
  const requests = new Map<string, number>();
  const occurrences: {
    url: string;
    label: string;
    start: number;
    end: number;
  }[] = [];

  function visit(node: RootContent) {
    if (nonProse.has(node.type)) return;
    if (node.type === "text") {
      const offset = node.position?.start.offset;
      if (offset === undefined) return;
      for (const match of matches(node)) {
        if (!requests.has(match.url) && requests.size >= maxReferenceLookups)
          continue;
        requests.set(match.url, match.number);
        occurrences.push({
          url: match.url,
          label: match.label,
          start: offset + match.start,
          end: offset + match.end,
        });
      }
    } else if ("children" in node) {
      node.children.forEach(visit);
    }
  }
  const document = parser.parse(body);
  document.children.forEach(visit);
  if (!requests.size) return body;

  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 3_000);
  const resolved = new Map<string, string>();
  try {
    await Promise.all(
      Array.from(requests, async ([url, number]) => {
        try {
          const { data } = await request<ReferenceSearch>(
            url,
            controller.signal,
          );
          const href = uniqueReferenceHref(data, number);
          if (href) resolved.set(url, href);
        } catch {
          // A reference lookup can fail independently of a valid body write.
        }
      }),
    );
  } finally {
    clearTimeout(timeout);
  }

  let result = body;
  // Source offsets belong to the original text. Working back to front keeps
  // the following replacement ranges valid.
  for (const { url, label, start, end } of occurrences.reverse()) {
    const href = resolved.get(url);
    if (!href) continue;
    size += bytes.encode(href).byteLength + 4;
    if (size > 65_536) return body;
    result = `${result.slice(0, start)}[${label}](${href})${result.slice(end)}`;
  }
  return result;
}
