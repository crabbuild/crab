import Markdown, { type Options } from "react-markdown";
import remarkGfm from "remark-gfm";
import type { Element, Root, RootContent } from "hast";
import { useContext, type ReactNode } from "react";
import { Link } from "@/components/navigation/link";
import { useRequest, type Repository } from "@/lib/api";
import { MarkdownCodeBlock } from "./markdown-code";
import {
  DiscussionCodeTheme,
  DiscussionRepository,
} from "./repository-context";
import { type ReferenceSearch } from "./editor-suggestions";
import {
  maxReferenceLookups,
  referenceMatcher,
  uniqueReferenceHref,
} from "./discussion-references";

function element(
  tagName: string,
  children: Element["children"],
  properties: Element["properties"] = {},
): Element {
  return { type: "element", tagName, properties, children };
}

function discussionBlocks() {
  return (root: Root) => {
    function visit(parent: Root | Element) {
      const nodes: RootContent[] = [];
      const details: { summary: Element; children: typeof nodes }[] = [];
      for (const node of parent.children) {
        const current = details.at(-1)?.children ?? nodes;
        if (node.type === "raw") {
          // Recognize only the Markdown details form. Raw attributes and HTML
          // inside the heading never become DOM nodes.
          const title =
            /^<details>[ \t]*\r?\n[ \t]*<summary>([^<>\r\n]+)<\/summary>$/.exec(
              node.value.trim(),
            )?.[1];
          if (title) {
            details.push({
              summary: element("summary", [{ type: "text", value: title }]),
              children: [],
            });
            continue;
          }
          if (node.value.trim() === "</details>" && details.length) {
            const section = details.pop()!;
            const output = details.at(-1)?.children ?? nodes;
            output.push(
              element("details", [
                section.summary,
                ...(section.children as Element["children"]),
              ]),
            );
            continue;
          }
        }
        if (node.type === "element") visit(node);
        if (node.type === "element" && node.tagName === "blockquote") {
          const paragraph = node.children.find(
            (child) => child.type !== "text" || child.value.trim(),
          );
          const marker =
            paragraph?.type === "element" && paragraph.tagName === "p"
              ? paragraph.children[0]
              : null;
          if (marker?.type === "text") {
            const alert =
              /^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\](?:\r?\n|$)/.exec(
                marker.value,
              );
            if (alert) {
              const kind = alert[1].toLowerCase();
              marker.value = marker.value.slice(alert[0].length);
              node.tagName = "aside";
              node.properties = {
                className: ["markdown-alert"],
                dataKind: kind,
              };
              node.children.unshift(
                element(
                  "p",
                  [
                    {
                      type: "text",
                      value: kind[0].toUpperCase() + kind.slice(1),
                    },
                  ],
                  { className: ["markdown-alert-title"] },
                ),
              );
            }
          }
        }
        current.push(node);
      }
      // An unfinished draft keeps its body visible in Preview.
      for (const section of details) nodes.push(...section.children);
      parent.children = nodes as Element["children"];
    }
    visit(root);
  };
}

type ReferenceContext = {
  source: string;
  repository: Pick<Repository, "id" | "owner" | "name"> | null;
};

const uuid = "[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}";
const attachmentImage = new RegExp(
  `^/api/discussion-attachments/${uuid}/${uuid}$`,
);

function discussionReferences({ source, repository }: ReferenceContext) {
  return (root: Root) => {
    const requests = new Set<string>();
    const matches = referenceMatcher(source, repository);
    function visit(parent: Root | Element) {
      const nodes: RootContent[] = [];
      for (const node of parent.children) {
        if (
          node.type === "element" &&
          !["a", "code", "pre"].includes(node.tagName)
        ) {
          visit(node);
        }
        if (node.type !== "text") {
          nodes.push(node);
          continue;
        }
        let offset = 0;
        for (const match of matches(node)) {
          if (!requests.has(match.url) && requests.size >= maxReferenceLookups)
            continue;
          requests.add(match.url);
          nodes.push(
            { type: "text", value: node.value.slice(offset, match.start) },
            element("span", [{ type: "text", value: match.label }], {
              dataCrabReference: match.url,
              dataCrabNumber: match.number,
            }),
          );
          offset = match.end;
        }
        nodes.push(
          offset ? { ...node, value: node.value.slice(offset) } : node,
        );
      }
      parent.children = nodes as Element["children"];
    }
    visit(root);
  };
}

function PlainReference({
  url,
  number,
  children,
}: {
  url: string;
  number: number;
  children: ReactNode;
}) {
  const { data } = useRequest<ReferenceSearch>(url);
  const href = uniqueReferenceHref(data, number);
  return href ? <a href={href}>{children}</a> : <>{children}</>;
}

const remarkPlugins = [remarkGfm];

export function DiscussionMarkdown({ children }: { children: string }) {
  const appearance = useContext(DiscussionCodeTheme);
  const repository = useContext(DiscussionRepository);
  const rehypePlugins: NonNullable<Options["rehypePlugins"]> = [
    discussionBlocks,
    [discussionReferences, { source: children, repository }],
  ];
  return (
    <div className="discussion-markdown">
      <Markdown
        skipHtml
        remarkPlugins={remarkPlugins}
        rehypePlugins={rehypePlugins}
        components={{
          pre: ({ children }) =>
            appearance ? (
              <MarkdownCodeBlock {...appearance}>{children}</MarkdownCodeBlock>
            ) : (
              <pre>{children}</pre>
            ),
          img: ({ src, alt }) =>
            typeof src === "string" && attachmentImage.test(src) ? (
              <img
                className="discussion-attachment-image"
                src={src}
                alt={alt || "View attachment"}
                loading="lazy"
              />
            ) : (
              <a
                href={typeof src === "string" ? src : undefined}
                rel="noreferrer"
              >
                {alt || "View image"}
              </a>
            ),
          span: ({ node, children, ...props }) => {
            const url = node?.properties.dataCrabReference;
            const number = node?.properties.dataCrabNumber;
            return typeof url === "string" && typeof number === "number" ? (
              <PlainReference url={url} number={number}>
                {children}
              </PlainReference>
            ) : (
              <span {...props}>{children}</span>
            );
          },
          a: ({ href, children }) =>
            href &&
            !href.includes("\\") &&
            /^\/(?:-\/users\/[\da-f-]+|[^/]+\/[^/]+\/(?:issues|pull)\/\d+)$/i.test(
              href,
            ) ? (
              <Link href={href}>{children}</Link>
            ) : (
              <a href={href} rel="noreferrer">
                {children}
              </a>
            ),
        }}
      >
        {children || "_No description provided._"}
      </Markdown>
    </div>
  );
}
