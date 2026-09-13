import {
  Children,
  Suspense,
  isValidElement,
  lazy,
  type ReactNode,
} from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { endpoint, repoHref, type Repository } from "./api";
import { Link } from "./ui";
import { enableCodeKeyboardScroll, type CodeThemes } from "./code-theme";

const HighlightedFile = lazy(() =>
  import("@pierre/diffs/react").then((module) => ({ default: module.File })),
);

function encodePathComponent(value: string) {
  return Array.from(new TextEncoder().encode(value), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function inlineRaster(value?: string) {
  return /\.(?:png|jpe?g|gif|webp)$/i.test(value?.split(/[?#]/, 1)[0] ?? "");
}

function repositoryTarget(directory: string, value?: string) {
  if (
    !value ||
    value.startsWith("#") ||
    value.startsWith("/") ||
    /^[a-z][a-z\d+.-]*:/i.test(value)
  )
    return null;
  const [pathAndQuery, fragment] = value.split("#", 2);
  const encodedPath = pathAndQuery.split("?", 1)[0];
  let decodedPath: string;
  try {
    decodedPath = decodeURIComponent(encodedPath);
  } catch {
    return null;
  }
  const components = directory ? directory.split("2f") : [];
  for (const component of decodedPath.split("/")) {
    if (!component || component === ".") continue;
    if (component === "..") {
      if (!components.length) return null;
      components.pop();
    } else {
      components.push(encodePathComponent(component));
    }
  }
  if (!components.length) return null;
  return {
    path: components.join("2f"),
    kind: decodedPath.endsWith("/") ? "Tree" : "Blob",
    fragment: fragment ? `#${fragment}` : "",
  };
}

export function RepositoryMarkdown({
  repo,
  rev,
  directory,
  children,
  className,
  theme,
  codeThemes,
}: {
  repo: Repository;
  rev: string;
  directory: string;
  children: string;
  className?: string;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  return (
    <div className={`discussion-markdown${className ? ` ${className}` : ""}`}>
      <Markdown
        skipHtml
        remarkPlugins={[remarkGfm]}
        components={{
          pre: ({ children }) => (
            <MarkdownCodeBlock theme={theme} codeThemes={codeThemes}>
              {children}
            </MarkdownCodeBlock>
          ),
          a: ({ href, children }) => {
            const target = repositoryTarget(directory, href);
            return target ? (
              <Link
                href={`${repoHref(repo, {
                  rev,
                  path: target.path,
                  kind: target.kind,
                })}${target.fragment}`}
              >
                {children}
              </Link>
            ) : (
              <a href={href} rel="noreferrer">
                {children}
              </a>
            );
          },
          img: ({ src, alt }) => {
            const target = repositoryTarget(directory, src);
            if (!target)
              return (
                <a
                  href={typeof src === "string" ? src : undefined}
                  rel="noreferrer"
                >
                  {alt || "View image"}
                </a>
              );
            const blob = endpoint(repo, "blob", {
              rev,
              path_hex: target.path,
            });
            return inlineRaster(src) ? (
              <img
                src={endpoint(repo, "asset", {
                  rev,
                  path_hex: target.path,
                })}
                alt={alt ?? ""}
                loading="lazy"
              />
            ) : (
              <a href={blob}>{alt || "View image"}</a>
            );
          },
        }}
      >
        {children}
      </Markdown>
    </div>
  );
}

function MarkdownCodeBlock({
  children,
  theme,
  codeThemes,
}: {
  children: ReactNode;
  theme: "light" | "dark";
  codeThemes: CodeThemes;
}) {
  const code = Children.count(children) === 1 ? Children.only(children) : null;
  if (!isValidElement<{ className?: string; children?: ReactNode }>(code))
    return <pre>{children}</pre>;
  const language = /^language-(\S+)$/.exec(code.props.className ?? "")?.[1];
  if (!language) return <pre>{children}</pre>;
  const contents = String(code.props.children ?? "").replace(/\n$/, "");
  return (
    <Suspense fallback={<pre>{children}</pre>}>
      <HighlightedFile
        key={`${theme}:${codeThemes.light}:${codeThemes.dark}`}
        className="markdown-code-block"
        file={{ name: `snippet.${language}`, contents, lang: language }}
        options={{
          theme: codeThemes,
          themeType: theme,
          preferredHighlighter: "shiki-js",
          disableFileHeader: true,
          disableLineNumbers: true,
          overflow: "scroll",
          onPostRender: enableCodeKeyboardScroll,
        }}
      />
    </Suspense>
  );
}
