import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";

export function DiscussionMarkdown({ children }: { children: string }) {
  return (
    <div className="discussion-markdown">
      <Markdown
        skipHtml
        remarkPlugins={[remarkGfm]}
        components={{
          img: ({ src, alt }) => (
            <a
              href={typeof src === "string" ? src : undefined}
              rel="noreferrer"
            >
              {alt || "View image"}
            </a>
          ),
          a: ({ href, children }) => (
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
