import { useLayoutEffect, useState, type ReactNode } from "react";
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup,
} from "@/components/ui/resizable";

export function FileWorkspace({
  children,
  content,
  symbols,
}: {
  children: ReactNode;
  content: HTMLElement | null;
  symbols: ReactNode;
}) {
  const [workspace, setWorkspace] = useState<HTMLDivElement | null>(null);
  const [stacked, setStacked] = useState(
    () => matchMedia("(max-width: 900px)").matches,
  );
  useLayoutEffect(() => {
    const parent = workspace?.parentElement;
    if (!workspace || !parent) return;
    const navigation = parent.querySelector<HTMLElement>(".file-navigation");
    const toolbar = content?.previousElementSibling;
    const separator = workspace.querySelector<HTMLElement>(
      ".code-symbols-resizer",
    );
    const grip = separator?.querySelector<HTMLElement>(":scope > div");
    const narrow = matchMedia("(max-width: 900px)");
    let frame = 0;
    const measure = () => {
      const width = workspace.clientWidth;
      if (width === 0) return;
      // An open explorer can leave a narrow source column on a wide screen.
      setStacked(width < 700 || narrow.matches);
      const bounds = workspace.getBoundingClientRect();
      const inset = Math.max(
        0,
        navigation?.getBoundingClientRect().bottom ?? 0,
      );
      const bottom = Math.min(innerHeight, bounds.bottom);
      workspace.style.setProperty(
        "--workspace-available-height",
        `${Math.max(0, bottom - Math.max(inset, bounds.top))}px`,
      );
      if (!content || !separator || !grip) return;
      const source = content.getBoundingClientRect();
      if (source.width === 0) return;
      // The content edge, not a taller Symbols list or page, owns the grip.
      // Exclude the sticky toolbar so the grip cannot overlap either header.
      const top = Math.max(
        source.top,
        inset,
        toolbar?.getBoundingClientRect().bottom ?? 0,
      );
      const visibleHeight = Math.max(
        0,
        Math.min(innerHeight, source.bottom) - top,
      );
      separator.style.height = `${Math.max(0, source.bottom - bounds.top)}px`;
      grip.style.top = `${top + visibleHeight / 2 - bounds.top}px`;
      grip.style.maxHeight = `${visibleHeight}px`;
    };
    const schedule = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(measure);
    };
    measure();
    const observer = new ResizeObserver(schedule);
    for (const node of [workspace, parent, navigation, toolbar, content]) {
      if (node) observer.observe(node);
    }
    window.addEventListener("resize", schedule);
    window.addEventListener("scroll", schedule, { passive: true });
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
      window.removeEventListener("resize", schedule);
      window.removeEventListener("scroll", schedule);
      workspace.style.removeProperty("--workspace-available-height");
      separator?.style.removeProperty("height");
      grip?.style.removeProperty("top");
      grip?.style.removeProperty("max-height");
    };
  }, [workspace, content]);

  if (!symbols || stacked)
    return (
      <div
        ref={setWorkspace}
        className="file-source-workspace"
        data-stacked={stacked || undefined}
      >
        {children}
        {symbols}
      </div>
    );

  // Intrinsic height keeps short files compact. Visible overflow lets the
  // document own scrolling and preserves both sticky column headers.
  return (
    <ResizablePanelGroup
      elementRef={setWorkspace}
      orientation="horizontal"
      className="file-source-workspace"
      style={{ height: "auto", overflow: "visible" }}
    >
      <ResizablePanel minSize="40%" style={{ overflow: "visible" }}>
        {children}
      </ResizablePanel>
      <ResizableHandle
        withHandle
        className="code-symbols-resizer"
        aria-label="Resize symbols pane"
      />
      <ResizablePanel
        defaultSize={320}
        groupResizeBehavior="preserve-pixel-size"
        minSize={200}
        maxSize="60%"
        style={{ overflow: "visible" }}
      >
        {symbols}
      </ResizablePanel>
    </ResizablePanelGroup>
  );
}
