import { Fragment } from "react";
import {
  CircleCheckIcon,
  CircleDotIcon,
  FlagIcon,
  GitBranchIcon,
  GitPullRequestIcon,
  HashIcon,
  MessageSquareIcon,
  SearchIcon,
  SquareIcon,
  TagIcon,
  UserRoundIcon,
  type LucideIcon,
} from "lucide-react";

const icons = new Map<string, LucideIcon>([
  ["is", CircleDotIcon],
  ["state", CircleDotIcon],
  ["draft", GitPullRequestIcon],
  ["base", GitBranchIcon],
  ["head", GitBranchIcon],
  ["status", CircleCheckIcon],
  ["label", TagIcon],
  ["milestone", FlagIcon],
  ["type", SquareIcon],
  ["author", UserRoundIcon],
  ["assignee", UserRoundIcon],
  ["commenter", UserRoundIcon],
  ["review", GitPullRequestIcon],
  ["review-requested", GitPullRequestIcon],
  ["team-review-requested", GitPullRequestIcon],
  ["reviewed-by", GitPullRequestIcon],
  ["review-involves", GitPullRequestIcon],
  ["user-review-requested", GitPullRequestIcon],
  ["reason", CircleDotIcon],
  ["comments", MessageSquareIcon],
  ["reactions", HashIcon],
  ["interactions", HashIcon],
  ["no", SquareIcon],
  ["in", SearchIcon],
]);

function qualifier(atom: string) {
  return /^-?([a-z][a-z-]*):/i.exec(atom);
}

export function querySuggestionIcon(value: string) {
  return icons.get(qualifier(value)?.[1].toLowerCase() ?? "") ?? SearchIcon;
}

export function QuerySyntax({ value }: { value: string }) {
  // Match quoted pieces within an atom, including unfinished quotes during editing.
  const tokens =
    value.match(/\s+|[()]|(?:[^\s()"]|"(?:\\.?|[^"\\])*"?)+/gu) ?? [];
  let offset = 0;
  return tokens.map((token) => {
    const start = offset;
    offset += token.length;
    const prefix = qualifier(token);
    if (prefix && icons.has(prefix[1].toLowerCase())) {
      return (
        <Fragment key={start}>
          <span data-query-part="qualifier">{prefix[0]}</span>
          <span data-query-part="value">{token.slice(prefix[0].length)}</span>
        </Fragment>
      );
    }
    return /^(?:AND|OR|[()])$/.test(token) ? (
      <span key={start} data-query-part="operator">
        {token}
      </span>
    ) : (
      token
    );
  });
}
