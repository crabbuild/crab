import { useEffect, useRef } from "react";
import { useLocation } from "@/lib/api";

export function useDiscussionPermalink() {
  const location = useLocation();
  const hash = new URL(location, window.location.origin).hash;
  const match = /^#(issuecomment|pullrequestreview)-([1-9]\d*)$/.exec(hash);
  const number = Number(match?.[2]);
  return match && Number.isSafeInteger(number)
    ? { kind: match[1], number, id: hash.slice(1) }
    : undefined;
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
