import { useEffect, useRef } from "react";
import { useLocation } from "@/lib/api";

export function useDiscussionPermalink() {
  const location = useLocation();
  const hash = new URL(location, window.location.origin).hash;
  const match = /^#(issuecomment|pullrequestreview|event)-([1-9]\d*)$/.exec(
    hash,
  );
  const number = Number(match?.[2]);
  if (!match || !Number.isSafeInteger(number)) return undefined;
  const source =
    match[1] === "issuecomment"
      ? "comment"
      : match[1] === "pullrequestreview"
        ? "review"
        : "activity";
  return { id: hash.slice(1), query: `${source}:${number}` };
}

export function useContributionFocus(id: string | undefined, ready: boolean) {
  const target = useRef<{ id?: string; focused: boolean }>({ focused: false });
  useEffect(() => {
    if (target.current.id !== id) target.current = { id, focused: false };
    if (!id || !ready || target.current.focused) return;
    const element = document.getElementById(id);
    if (!element) return;
    element.scrollIntoView({ block: "center" });
    element.focus({ preventScroll: true });
    target.current.focused = true;
  }, [id, ready]);
}
