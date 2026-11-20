import { useLayoutEffect } from "react";

export function useWorkspaceHeight(workspace: HTMLElement | null) {
  useLayoutEffect(() => {
    if (!workspace) return;
    let frame = 0;
    const measure = () => {
      const bounds = workspace.getBoundingClientRect();
      if (bounds.width === 0 || bounds.height === 0) return;
      // Sticky panes must shrink when the footer enters their workspace.
      // A full viewport height would push their headers above the viewport.
      const top = Math.max(0, bounds.top);
      const bottom = Math.min(window.innerHeight, bounds.bottom);
      workspace.style.setProperty(
        "--workspace-available-height",
        `${Math.max(0, bottom - top)}px`,
      );
      workspace.style.setProperty("--workspace-visible-top", `${top}px`);
    };
    const schedule = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(measure);
    };
    measure();
    const observer = new ResizeObserver(schedule);
    observer.observe(workspace);
    window.addEventListener("resize", schedule);
    window.addEventListener("scroll", schedule, { passive: true });
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
      window.removeEventListener("resize", schedule);
      window.removeEventListener("scroll", schedule);
      workspace.style.removeProperty("--workspace-available-height");
      workspace.style.removeProperty("--workspace-visible-top");
    };
  }, [workspace]);
}
