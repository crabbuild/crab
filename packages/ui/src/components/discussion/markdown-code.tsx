import {
  Children,
  Suspense,
  isValidElement,
  lazy,
  type ReactNode,
} from "react";
import { enableCodeKeyboardScroll, type CodeThemes } from "@/lib/code-theme";

const HighlightedFile = lazy(() =>
  import("@pierre/diffs/react").then((module) => ({ default: module.File })),
);

export function MarkdownCodeBlock({
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
