import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { endpoint, repoHref, type Repository } from "./api";
import { Link } from "./ui";

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
}: {
  repo: Repository;
  rev: string;
  directory: string;
  children: string;
  className?: string;
}) {
  return (
    <div className={`discussion-markdown${className ? ` ${className}` : ""}`}>
      <Markdown
        skipHtml
        remarkPlugins={[remarkGfm]}
        components={{
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
