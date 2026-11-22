export type MarkdownFormat =
  | "heading"
  | "bold"
  | "italic"
  | "quote"
  | "code"
  | "link"
  | "unordered-list"
  | "ordered-list"
  | "task-list";

export type MarkdownEdit = {
  value: string;
  start: number;
  end: number;
};

function insertMarkdown(
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
  const blockStart = start === 0 ? 0 : value.lastIndexOf("\n", start - 1) + 1;
  const newline = value.indexOf(
    "\n",
    end > start && value[end - 1] === "\n" ? end - 1 : end,
  );
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

export function formatMarkdown(
  value: string,
  start: number,
  end: number,
  format: MarkdownFormat,
): MarkdownEdit {
  if (format === "heading") return prefixLines(value, start, end, () => "### ");
  if (format === "bold")
    return insertMarkdown(value, start, end, "**", "**", "bold text");
  if (format === "italic")
    return insertMarkdown(value, start, end, "_", "_", "italic text");
  if (format === "quote") return prefixLines(value, start, end, () => "> ");
  if (format === "code") {
    const selected = value.slice(start, end);
    return selected.includes("\n")
      ? insertMarkdown(value, start, end, "```\n", "\n```", "code")
      : insertMarkdown(value, start, end, "`", "`", "code");
  }
  if (format === "link") {
    const edit = insertMarkdown(value, start, end, "[", "](url)", "link text");
    return start === end
      ? edit
      : { ...edit, start: edit.end + 2, end: edit.end + 5 };
  }
  if (format === "unordered-list")
    return prefixLines(value, start, end, () => "- ");
  if (format === "ordered-list")
    return prefixLines(value, start, end, (index) => `${index + 1}. `);
  return prefixLines(value, start, end, () => "- [ ] ");
}

export function linkSelection(
  value: string,
  start: number,
  end: number,
  pasted: string,
): MarkdownEdit | null {
  if (start === end || !/^https?:\/\/\S+$/i.test(pasted)) return null;
  let href: string;
  try {
    href = new URL(pasted).href.replaceAll("(", "%28").replaceAll(")", "%29");
  } catch {
    return null;
  }
  const selected = value.slice(start, end).replace(/[\\[\]]/g, "\\$&");
  const link = `[${selected}](${href})`;
  const caret = start + link.length;
  return {
    value: value.slice(0, start) + link + value.slice(end),
    start: caret,
    end: caret,
  };
}
