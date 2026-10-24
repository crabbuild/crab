import {
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
  ListIcon,
  AtSignIcon,
  QuoteIcon,
  ListTodoIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup } from "@/components/ui/field";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { DiscussionMarkdown } from "@/components/discussion/markdown";

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
      icon: ListIcon,
    },
    {
      format: "ordered-list",
      label: "Add a numbered list",
      icon: ListOrderedIcon,
    },
    { format: "task-list", label: "Add a task list", icon: ListTodoIcon },
  ],
  [{ format: "mention", label: "Mention a user", icon: AtSignIcon }],
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
    <Tabs
      className="discussion-editor gap-0"
      value={preview ? "preview" : "write"}
      onValueChange={(value) => setPreview(value === "preview")}
    >
      <div className="editor-header">
        <TabsList variant="editor" aria-label={`${label} mode`}>
          <TabsTrigger value="write">Write</TabsTrigger>
          <TabsTrigger value="preview">Preview</TabsTrigger>
        </TabsList>
        <div
          className="editor-toolbar"
          role="toolbar"
          aria-label={`${label} formatting`}
          hidden={preview}
        >
          {MARKDOWN_TOOLS.map((group, groupIndex) => (
            <span className="editor-tool-group" key={group[0].format}>
              {groupIndex > 0 && (
                <Separator orientation="vertical" className="mx-1" />
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
                  <Button
                    key={tool.format}
                    type="button"
                    variant="ghost"
                    size="icon-sm"
                    aria-label={tool.label}
                    aria-keyshortcuts={shortcut}
                    title={tool.label}
                    disabled={disabled}
                    onClick={() => applyFormat(tool.format)}
                  >
                    <Icon data-icon="inline-start" />
                  </Button>
                );
              })}
            </span>
          ))}
        </div>
      </div>
      <TabsContent value="preview" className="editor-preview" tabIndex={0}>
        <DiscussionMarkdown>{value}</DiscussionMarkdown>
      </TabsContent>
      <TabsContent
        value="write"
        className="editor-write"
        forceMount
        hidden={preview}
      >
        <FieldGroup>
          <Field data-disabled={disabled}>
            <Textarea
              ref={textarea}
              aria-label={label}
              aria-describedby={`${id}-help`}
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
          </Field>
        </FieldGroup>
      </TabsContent>
      <p id={`${id}-help`} className="editor-help">
        Markdown is supported. External images appear as links.
      </p>
    </Tabs>
  );
}
