import { useState } from "react";
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

export function Editor({
  id,
  label,
  value,
  onChange,
  disabled,
  required = false,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  disabled: boolean;
  required?: boolean;
}) {
  const [preview, setPreview] = useState(false);
  return (
    <div className="discussion-editor">
      <div
        className="editor-tabs"
        role="tablist"
        aria-label={`${label} mode`}
        onKeyDown={(event) => {
          if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key))
            return;
          event.preventDefault();
          const next =
            event.key === "Home"
              ? false
              : event.key === "End"
                ? true
                : !preview;
          setPreview(next);
          document
            .getElementById(`${id}-${next ? "preview" : "write"}-tab`)
            ?.focus();
        }}
      >
        <button
          type="button"
          role="tab"
          id={`${id}-write-tab`}
          aria-controls={`${id}-write`}
          aria-selected={!preview}
          tabIndex={preview ? -1 : 0}
          onClick={() => setPreview(false)}
        >
          Write
        </button>
        <button
          type="button"
          role="tab"
          id={`${id}-preview-tab`}
          aria-controls={`${id}-preview`}
          aria-selected={preview}
          tabIndex={preview ? 0 : -1}
          onClick={() => setPreview(true)}
        >
          Preview
        </button>
      </div>
      {preview ? (
        <div
          id={`${id}-preview`}
          role="tabpanel"
          aria-labelledby={`${id}-preview-tab`}
          className="editor-preview"
        >
          <DiscussionMarkdown>{value}</DiscussionMarkdown>
        </div>
      ) : (
        <div
          id={`${id}-write`}
          role="tabpanel"
          aria-labelledby={`${id}-write-tab`}
        >
          <label className="sr-only" htmlFor={id}>
            {label}
          </label>
          <textarea
            id={id}
            value={value}
            onChange={(event) => onChange(event.target.value)}
            disabled={disabled}
            required={required}
            maxLength={65_536}
            rows={8}
            placeholder="Add context, ask a question, or share an update…"
          />
        </div>
      )}
      <p className="editor-help muted">
        Markdown is supported. External images appear as links.
      </p>
    </div>
  );
}
export function Failure({ message }: { message?: string }) {
  return message ? (
    <p className="notice error" role="alert">
      {message} Your draft is still in this form.
    </p>
  ) : null;
}
