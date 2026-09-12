import {
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
  type KeyboardEvent,
  type Ref,
} from "react";
import {
  BoldIcon,
  CodeIcon,
  HeadingIcon,
  ItalicIcon,
  LinkIcon,
  ListOrderedIcon,
  ListUnorderedIcon,
  MentionIcon,
  QuoteIcon,
  TasklistIcon,
} from "@primer/octicons-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";

type MarkdownFormat =
  | "heading"
  | "bold"
  | "italic"
  | "quote"
  | "code"
  | "link"
  | "unordered-list"
  | "ordered-list"
  | "task-list"
  | "mention";

type MarkdownEdit = {
  value: string;
  start: number;
  end: number;
};

const MARKDOWN_TOOLS = [
  [
    { format: "heading", label: "Add heading", icon: HeadingIcon },
    { format: "bold", label: "Add bold text", icon: BoldIcon },
    { format: "italic", label: "Add italic text", icon: ItalicIcon },
  ],
  [
    { format: "quote", label: "Insert a quote", icon: QuoteIcon },
    { format: "code", label: "Insert code", icon: CodeIcon },
    { format: "link", label: "Add a link", icon: LinkIcon },
  ],
  [
    {
      format: "unordered-list",
      label: "Add a bulleted list",
      icon: ListUnorderedIcon,
    },
    {
      format: "ordered-list",
      label: "Add a numbered list",
      icon: ListOrderedIcon,
    },
    { format: "task-list", label: "Add a task list", icon: TasklistIcon },
  ],
  [{ format: "mention", label: "Mention a user", icon: MentionIcon }],
] as const;

function wrapSelection(
  value: string,
  start: number,
  end: number,
  before: string,
  after: string,
  placeholder = "",
): MarkdownEdit {
  const selected = value.slice(start, end) || placeholder;
  const next = `${value.slice(0, start)}${before}${selected}${after}${value.slice(end)}`;
  const selectionStart = start + before.length;
  return {
    value: next,
    start: selectionStart,
    end: selectionStart + selected.length,
  };
}

function prefixLines(
  value: string,
  start: number,
  end: number,
  prefix: (index: number) => string,
): MarkdownEdit {
  const blockStart = value.lastIndexOf("\n", Math.max(0, start - 1)) + 1;
  const newline = value.indexOf("\n", end);
  const blockEnd = newline === -1 ? value.length : newline;
  const lines = value.slice(blockStart, blockEnd).split("\n");
  const formatted = lines
    .map((line, index) => `${prefix(index)}${line}`)
    .join("\n");
  const next = `${value.slice(0, blockStart)}${formatted}${value.slice(blockEnd)}`;
  if (start === end) {
    const offset = prefix(0).length;
    return { value: next, start: start + offset, end: end + offset };
  }
  return { value: next, start: blockStart, end: blockStart + formatted.length };
}

function formatMarkdown(
  value: string,
  start: number,
  end: number,
  format: MarkdownFormat,
): MarkdownEdit {
  if (format === "heading") return prefixLines(value, start, end, () => "### ");
  if (format === "bold")
    return wrapSelection(value, start, end, "**", "**", "bold text");
  if (format === "italic")
    return wrapSelection(value, start, end, "_", "_", "italic text");
  if (format === "quote") return prefixLines(value, start, end, () => "> ");
  if (format === "code") {
    const selected = value.slice(start, end);
    return selected.includes("\n")
      ? wrapSelection(value, start, end, "```\n", "\n```", "code")
      : wrapSelection(value, start, end, "`", "`", "code");
  }
  if (format === "link")
    return value.slice(start, end)
      ? wrapSelection(value, start, end, "[", "](url)")
      : wrapSelection(value, start, end, "[", "](url)", "link text");
  if (format === "unordered-list")
    return prefixLines(value, start, end, () => "- ");
  if (format === "ordered-list")
    return prefixLines(value, start, end, (index) => `${index + 1}. `);
  if (format === "task-list")
    return prefixLines(value, start, end, () => "- [ ] ");
  const next = `${value.slice(0, start)}@${value.slice(end)}`;
  return { value: next, start: start + 1, end: start + 1 };
}

export function useReturnFocus<T extends HTMLElement>(active: boolean) {
  const target = useRef<T>(null);
  const previous = useRef(active);
  useEffect(() => {
    // Removed/disabled controls leave focus on the body. Preserve any deliberate
    // move elsewhere while an edit or request was in progress.
    if (previous.current && !active && document.activeElement === document.body)
      target.current?.focus();
    previous.current = active;
  }, [active]);
  return target;
}

// Retain the key for an unchanged submission so an ambiguous retry cannot duplicate it.
export function useSubmission() {
  const current = useRef({ body: "", id: crypto.randomUUID() });
  return (input: object) => {
    const body = JSON.stringify(input);
    if (body !== current.current.body)
      current.current = { body, id: crypto.randomUUID() };
    return current.current.id;
  };
}

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
  autoFocus = false,
  ref,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  disabled: boolean;
  required?: boolean;
  autoFocus?: boolean;
  ref?: Ref<HTMLTextAreaElement>;
}) {
  const [preview, setPreview] = useState(false);
  const textarea = useRef<HTMLTextAreaElement>(null);
  useImperativeHandle(ref, () => textarea.current as HTMLTextAreaElement);

  function applyFormat(format: MarkdownFormat) {
    const input = textarea.current;
    if (!input || disabled) return;
    const edit = formatMarkdown(
      value,
      input.selectionStart,
      input.selectionEnd,
      format,
    );
    onChange(edit.value);
    requestAnimationFrame(() => {
      input.focus();
      input.setSelectionRange(edit.start, edit.end);
    });
  }

  function formatShortcut(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (!(event.ctrlKey || event.metaKey) || event.altKey) return;
    const format =
      event.key.toLowerCase() === "b"
        ? "bold"
        : event.key.toLowerCase() === "i"
          ? "italic"
          : event.key.toLowerCase() === "k"
            ? "link"
            : null;
    if (!format) return;
    event.preventDefault();
    applyFormat(format);
  }

  return (
    <div className="discussion-editor">
      <div className="editor-header">
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
        <div
          className="editor-toolbar"
          role="toolbar"
          aria-label={`${label} formatting`}
          hidden={preview}
        >
          {MARKDOWN_TOOLS.map((group, groupIndex) => (
            <span className="editor-tool-group" key={group[0].format}>
              {groupIndex > 0 && (
                <span className="editor-tool-divider" aria-hidden="true" />
              )}
              {group.map((tool) => {
                const Icon = tool.icon;
                const shortcut =
                  tool.format === "bold"
                    ? "Control+B Meta+B"
                    : tool.format === "italic"
                      ? "Control+I Meta+I"
                      : tool.format === "link"
                        ? "Control+K Meta+K"
                        : undefined;
                return (
                  <button
                    key={tool.format}
                    type="button"
                    className="editor-tool"
                    aria-label={tool.label}
                    aria-keyshortcuts={shortcut}
                    title={tool.label}
                    disabled={disabled}
                    onClick={() => applyFormat(tool.format)}
                  >
                    <Icon size={16} />
                  </button>
                );
              })}
            </span>
          ))}
        </div>
      </div>
      <div
        id={`${id}-preview`}
        role="tabpanel"
        aria-labelledby={`${id}-preview-tab`}
        className="editor-preview"
        hidden={!preview}
        tabIndex={0}
      >
        {preview && <DiscussionMarkdown>{value}</DiscussionMarkdown>}
      </div>
      <div
        id={`${id}-write`}
        role="tabpanel"
        aria-labelledby={`${id}-write-tab`}
        hidden={preview}
      >
        <label className="sr-only" htmlFor={id}>
          {label}
        </label>
        <textarea
          ref={textarea}
          autoFocus={autoFocus}
          id={id}
          value={value}
          onChange={(event) => onChange(event.target.value)}
          disabled={disabled}
          required={required}
          maxLength={65_536}
          rows={8}
          placeholder="Add context, ask a question, or share an update…"
          onKeyDown={formatShortcut}
        />
      </div>
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
