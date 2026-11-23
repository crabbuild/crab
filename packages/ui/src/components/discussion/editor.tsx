import {
  useContext,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type ReactNode,
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
  HashIcon,
  TerminalIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup } from "@/components/ui/field";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { DiscussionMarkdown } from "@/components/discussion/markdown";
import { Popover, PopoverAnchor } from "@/components/ui/popover";
import { CaretAnchor, EditorMenu } from "./editor-menu";
import {
  formatMarkdown,
  linkSelection,
  type MarkdownEdit,
  type MarkdownFormat,
} from "./editor-format";
import {
  hasCommandOptions,
  triggerAt,
  useEditorSuggestions,
  type CommandMenu,
  type Suggestion,
  type Trigger,
} from "./editor-suggestions";
import { DiscussionCsrf, DiscussionRepository } from "./repository-context";
import { EditorAttachments, useEditorAttachments } from "./editor-attachments";

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
  [
    {
      format: "command",
      label: "Insert a Markdown command",
      icon: TerminalIcon,
    },
    { format: "mention", label: "Mention a person or team", icon: AtSignIcon },
    {
      format: "reference",
      label: "Reference an issue or pull request",
      icon: HashIcon,
    },
  ],
] as const;

const menuTitles = {
  command: "Markdown commands",
  mention: "Mention a collaborator",
  reference: "Issues and pull requests",
  alert: "Alert type",
  code: "Code language",
  emoji: "Emoji",
  "saved-replies": "Saved replies",
} as const;

export function Editor({
  id,
  label,
  value,
  onChange,
  disabled,
  required = false,
  autoFocus = false,
  hint,
  attachments = true,
  ref,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  disabled: boolean;
  required?: boolean;
  autoFocus?: boolean;
  hint?: ReactNode;
  attachments?: boolean;
  ref?: Ref<HTMLTextAreaElement>;
}) {
  const [preview, setPreview] = useState(false);
  const [completion, setCompletion] = useState<{
    source: string;
    trigger: Trigger;
  } | null>(null);
  const [command, setCommand] = useState<CommandMenu | null>(null);
  const [active, setActive] = useState(0);
  const composing = useRef(false);
  const pendingSelection = useRef<MarkdownEdit | null>(null);
  const textarea = useRef<HTMLTextAreaElement>(null);
  const caretAnchor = useRef<HTMLSpanElement>(null);
  const repo = useContext(DiscussionRepository);
  const csrf = useContext(DiscussionCsrf);
  const canAttach = attachments && repo !== null && csrf !== null;
  const uploads = useEditorAttachments(
    canAttach ? repo : null,
    csrf ?? "",
    value,
    onChange,
  );
  useImperativeHandle(ref, () => textarea.current as HTMLTextAreaElement);

  const trigger =
    !disabled && !preview && completion?.source === value
      ? completion.trigger
      : null;
  const { items, loading, error } = useEditorSuggestions(
    repo,
    trigger,
    command,
  );
  const selected = Math.min(active, Math.max(0, items.length - 1));
  const menuId = `${id}-suggestions`;

  function closeMenu() {
    setCompletion(null);
    setCommand(null);
    setActive(0);
  }

  function backToCommands() {
    setCommand(null);
    setActive(0);
  }

  function updateMenu(input: HTMLTextAreaElement) {
    if (composing.current) return;
    const next = triggerAt(
      input.value,
      input.selectionStart,
      input.selectionEnd,
    );
    if (!next || (next.kind !== "command" && !repo)) {
      closeMenu();
      return;
    }
    if (completion?.source === input.value && trigger?.end === next.end) return;
    setCommand(null);
    setActive(0);
    setCompletion({ source: input.value, trigger: next });
  }

  useLayoutEffect(() => {
    const input = textarea.current;
    const edit = pendingSelection.current;
    if (!input || !edit) return;
    pendingSelection.current = null;
    if (edit.value === value) input.setSelectionRange(edit.start, edit.end);
  }, [value]);

  function applyEdit(edit: MarkdownEdit, openMenu = false) {
    const input = textarea.current;
    if (!input || disabled) return;
    input.focus();
    if (edit.value !== value) {
      pendingSelection.current = edit;
      onChange(edit.value);
    } else input.setSelectionRange(edit.start, edit.end);
    closeMenu();
    const next = openMenu && triggerAt(edit.value, edit.start, edit.end);
    if (next) setCompletion({ source: edit.value, trigger: next });
  }

  function applyFormat(format: MarkdownFormat | Trigger["kind"]) {
    const input = textarea.current;
    if (!input || disabled) return;
    const { selectionStart: start, selectionEnd: end } = input;
    if (
      format === "command" ||
      format === "mention" ||
      format === "reference"
    ) {
      const before = value.slice(0, start);
      const after = value.slice(end);
      const separator = format === "command" ? "\n\n" : " ";
      const marker =
        format === "command" ? "/" : format === "mention" ? "@" : "#";
      const prefix = before && !before.endsWith(separator[0]) ? separator : "";
      const suffix = after && !/^\s/u.test(after) ? separator : "";
      const caret = before.length + prefix.length + marker.length;
      applyEdit(
        {
          value: before + prefix + marker + suffix + after,
          start: caret,
          end: caret,
        },
        true,
      );
    } else applyEdit(formatMarkdown(value, start, end, format));
  }

  function choose(item: Suggestion) {
    if (!trigger) return;
    if (item.kind === "command" && !command && hasCommandOptions(item.id)) {
      setCommand(item.id);
      setActive(0);
      return;
    }
    const before = value.slice(0, trigger.start);
    const after = value.slice(trigger.end);
    const block = item.kind === "command" && command !== "emoji";
    const prefix =
      block && before.trim() && !/\n\s*\n[ \t]*$/.test(before) ? "\n" : "";
    const suffix = block
      ? after.startsWith("\n\n")
        ? ""
        : after.startsWith("\n")
          ? "\n"
          : "\n\n"
      : after && /^[\s.,;:!?)}\]]/.test(after)
        ? ""
        : " ";
    const insertion = prefix + item.text + suffix;
    const start =
      trigger.start +
      (item.cursor !== undefined
        ? prefix.length + item.cursor
        : item.selection
          ? prefix.length + item.text.indexOf(item.selection)
          : insertion.length);
    const end = start + (item.selection?.length ?? 0);
    applyEdit({ value: before + insertion + after, start, end });
  }

  function formatShortcut(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.nativeEvent.isComposing || composing.current) return;
    if (
      trigger &&
      (event.key === "Escape" || (command && event.key === "ArrowLeft"))
    ) {
      event.preventDefault();
      if (command) backToCommands();
      else closeMenu();
      return;
    }
    if (
      trigger &&
      items.length &&
      !event.altKey &&
      !event.ctrlKey &&
      !event.metaKey &&
      !event.shiftKey
    ) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        setActive(
          (selected + (event.key === "ArrowDown" ? 1 : -1) + items.length) %
            items.length,
        );
        return;
      }
      if (event.key === "Enter" || event.key === "Tab") {
        event.preventDefault();
        choose(items[selected]);
        return;
      }
    }
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
      onValueChange={(value) => {
        closeMenu();
        setPreview(value === "preview");
      }}
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
          aria-hidden={preview}
          inert={preview}
        >
          {MARKDOWN_TOOLS.map((group, groupIndex) => (
            <span className="editor-tool-group" key={group[0].format}>
              {groupIndex > 0 && (
                <Separator orientation="vertical" className="mx-1" />
              )}
              {group.map((tool) => {
                if (
                  !repo &&
                  (tool.format === "mention" || tool.format === "reference")
                )
                  return null;
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
                    onMouseDown={(event) => event.preventDefault()}
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
          <Field data-disabled={disabled} className="editor-input">
            <Popover
              open={!!trigger}
              onOpenChange={(open) => {
                if (!open) closeMenu();
              }}
            >
              <Textarea
                ref={textarea}
                aria-label={label}
                aria-describedby={`${id}-help`}
                aria-autocomplete="list"
                aria-controls={trigger ? menuId : undefined}
                aria-activedescendant={
                  trigger && items.length ? `${menuId}-${selected}` : undefined
                }
                autoFocus={autoFocus}
                id={id}
                value={value}
                onChange={(event) => {
                  onChange(event.currentTarget.value);
                  updateMenu(event.currentTarget);
                }}
                onSelect={(event) => updateMenu(event.currentTarget)}
                onBlur={closeMenu}
                onPaste={(event) => {
                  if (canAttach && event.clipboardData.files.length) {
                    event.preventDefault();
                    uploads.add(event.clipboardData.files);
                    return;
                  }
                  const { selectionStart, selectionEnd } = event.currentTarget;
                  const edit = linkSelection(
                    value,
                    selectionStart,
                    selectionEnd,
                    event.clipboardData.getData("text/plain").trim(),
                  );
                  if (
                    edit &&
                    edit.value.length <= event.currentTarget.maxLength
                  ) {
                    event.preventDefault();
                    applyEdit(edit);
                  }
                }}
                onDragOver={(event) => {
                  if (
                    canAttach &&
                    !disabled &&
                    event.dataTransfer.types.includes("Files")
                  )
                    event.preventDefault();
                }}
                onDrop={(event) => {
                  if (
                    canAttach &&
                    !disabled &&
                    event.dataTransfer.files.length
                  ) {
                    event.preventDefault();
                    uploads.add(event.dataTransfer.files);
                  }
                }}
                onCompositionStart={() => {
                  composing.current = true;
                  closeMenu();
                }}
                onCompositionEnd={(event) => {
                  composing.current = false;
                  updateMenu(event.currentTarget);
                }}
                disabled={disabled}
                required={required}
                maxLength={65_536}
                rows={8}
                placeholder="Add context, ask a question, or share an update…"
                onKeyDown={formatShortcut}
              />
              <PopoverAnchor asChild>
                <span
                  ref={caretAnchor}
                  className="editor-caret-anchor"
                  aria-hidden="true"
                />
              </PopoverAnchor>
              {trigger && (
                <>
                  <CaretAnchor
                    input={textarea}
                    value={value}
                    caret={trigger.end}
                    anchor={caretAnchor}
                  />
                  <EditorMenu
                    id={menuId}
                    label={
                      trigger.kind === "mention" && trigger.query.includes("/")
                        ? "Mention a team"
                        : menuTitles[command ?? trigger.kind]
                    }
                    items={items}
                    active={selected}
                    loading={loading}
                    error={error}
                    onChoose={choose}
                    onPoint={setActive}
                    onBack={command ? backToCommands : undefined}
                  />
                </>
              )}
            </Popover>
          </Field>
        </FieldGroup>
      </TabsContent>
      {canAttach && (
        <EditorAttachments
          controller={uploads}
          disabled={disabled || preview}
        />
      )}
      <p id={`${id}-help`} className="editor-help">
        <span>
          Markdown supported. External images appear as links.
          {hint && <> {hint}</>}
        </span>
        <span>
          <kbd>/</kbd> Commands
          {repo && (
            <>
              {" "}
              <kbd>@</kbd> Mention <kbd>#</kbd> Reference
            </>
          )}
        </span>
      </p>
    </Tabs>
  );
}
