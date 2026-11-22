import {
  ArrowLeftIcon,
  AtSignIcon,
  CodeIcon,
  GitMergeIcon,
  GitPullRequestIcon,
  CircleDotIcon,
  CircleCheckIcon,
  TerminalIcon,
  UsersIcon,
} from "lucide-react";
import { useLayoutEffect, type RefObject } from "react";
import { PopoverContent } from "@/components/ui/popover";
import { Button } from "@/components/ui/button";
import type { Suggestion } from "./editor-suggestions";

export function EditorMenu({
  id,
  label,
  items,
  active,
  loading,
  error,
  onChoose,
  onPoint,
  onBack,
}: {
  id: string;
  label: string;
  items: Suggestion[];
  active: number;
  loading: boolean;
  error?: string;
  onChoose: (item: Suggestion) => void;
  onPoint: (index: number) => void;
  onBack?: () => void;
}) {
  useLayoutEffect(() => {
    document
      .getElementById(`${id}-${active}`)
      ?.scrollIntoView({ block: "nearest" });
  }, [id, active]);
  const status = loading
    ? "Searching…"
    : error
      ? "Suggestions unavailable"
      : "No matches";
  return (
    <PopoverContent
      className="editor-menu"
      role="presentation"
      align="start"
      collisionPadding={8}
      onOpenAutoFocus={(event) => event.preventDefault()}
      onCloseAutoFocus={(event) => event.preventDefault()}
      onEscapeKeyDown={(event) => event.preventDefault()}
    >
      <div className="editor-menu-heading">
        {onBack && (
          <Button
            variant="ghost"
            size="icon-xs"
            aria-label="Back to Markdown commands"
            tabIndex={-1}
            onMouseDown={(event) => event.preventDefault()}
            onClick={onBack}
          >
            <ArrowLeftIcon aria-hidden="true" />
          </Button>
        )}
        <span aria-hidden="true">{label}</span>
      </div>
      <ul
        id={id}
        role="listbox"
        aria-label={label}
        aria-busy={loading}
        hidden={!items.length}
      >
        {items.map((item, index) => {
          const Icon =
            item.kind === "team"
              ? UsersIcon
              : item.kind === "mention"
                ? AtSignIcon
                : item.kind === "pull"
                  ? item.state === "merged"
                    ? GitMergeIcon
                    : GitPullRequestIcon
                  : item.kind === "issue"
                    ? item.state === "closed"
                      ? CircleCheckIcon
                      : CircleDotIcon
                    : item.id === "code"
                      ? CodeIcon
                      : TerminalIcon;
          return (
            <li
              key={item.id}
              id={`${id}-${index}`}
              role="option"
              aria-selected={index === active}
              onPointerMove={(event) => {
                if (event.pointerType === "mouse") onPoint(index);
              }}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => onChoose(item)}
            >
              {item.kind === "command" &&
              /\p{Extended_Pictographic}/u.test(item.text) ? (
                <span aria-hidden="true">{item.text}</span>
              ) : (
                <Icon aria-hidden="true" data-state={item.state} />
              )}
              <div>
                <strong>{item.label}</strong>
                {item.description && <span>{item.description}</span>}
              </div>
            </li>
          );
        })}
      </ul>
      {!items.length && (
        <p className="editor-menu-status" role="status">
          {status}
        </p>
      )}
    </PopoverContent>
  );
}

export function CaretAnchor({
  input,
  value,
  caret,
  anchor,
}: {
  input: RefObject<HTMLTextAreaElement | null>;
  value: string;
  caret: number;
  anchor: RefObject<HTMLSpanElement | null>;
}) {
  useLayoutEffect(() => {
    const textarea = input.current;
    const marker = anchor.current;
    if (!textarea || !marker) return;
    const mirror = document.createElement("div");
    const tail = document.createElement("span");
    // Keep text after the caret in the mirror so wrapped words break the same
    // way as in the textarea. The first fragment begins at the caret.
    mirror.append(value.slice(0, caret), tail);
    tail.textContent = value.slice(caret) || "\u200b";
    Object.assign(mirror.style, {
      position: "absolute",
      inset: "0 auto auto 0",
      visibility: "hidden",
      pointerEvents: "none",
      whiteSpace: "pre-wrap",
      overflowWrap: "break-word",
      boxSizing: "content-box",
      border: "0",
      height: "0",
    });
    marker.before(mirror);
    const position = () => {
      const style = getComputedStyle(textarea);
      Object.assign(mirror.style, {
        font: style.font,
        lineHeight: style.lineHeight,
        letterSpacing: style.letterSpacing,
        padding: style.padding,
        tabSize: style.tabSize,
        direction: style.direction,
        width: `${textarea.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)}px`,
      });
      const caretBox = tail.getClientRects()[0];
      if (!caretBox) return;
      const box = mirror.getBoundingClientRect();
      const left =
        caretBox.left - box.left + textarea.clientLeft - textarea.scrollLeft;
      const top =
        caretBox.top - box.top + textarea.clientTop - textarea.scrollTop;
      marker.style.left = `${Math.max(0, Math.min(left, textarea.clientWidth))}px`;
      marker.style.top = `${Math.max(0, Math.min(top, textarea.clientHeight))}px`;
      marker.style.height = `${caretBox.height}px`;
    };
    const observer = new ResizeObserver(position);
    observer.observe(textarea);
    textarea.addEventListener("scroll", position);
    position();
    return () => {
      textarea.removeEventListener("scroll", position);
      observer.disconnect();
      mirror.remove();
    };
  }, [input, value, caret, anchor]);
  return null;
}
