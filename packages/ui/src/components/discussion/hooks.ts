import { useEffect, useRef } from "react";

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
