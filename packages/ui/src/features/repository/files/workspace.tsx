import { useLayoutEffect, useState, type ReactNode } from "react";
import { useWorkspaceHeight } from "@/components/layout/use-workspace-height";
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup,
} from "@/components/ui/resizable";

export function FileWorkspace({
  children,
  symbols,
}: {
  children: ReactNode;
  symbols: ReactNode;
}) {
  const [workspace, setWorkspace] = useState<HTMLDivElement | null>(null);
  useWorkspaceHeight(workspace);
  const [stacked, setStacked] = useState(
    () => matchMedia("(max-width: 900px)").matches,
  );
  useLayoutEffect(() => {
    const parent = workspace?.parentElement;
    if (!workspace || !parent) return;
    // An open explorer can leave a narrow file column on a wide screen.
    // Stack before the Symbols pane squeezes navigation out of that column.
    const update = () => {
      const width = workspace.clientWidth;
      if (width === 0) return;
      setStacked(width < 700 || matchMedia("(max-width: 900px)").matches);
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(parent);
    return () => observer.disconnect();
  }, [workspace]);

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

  // Visible overflow preserves document scrolling and the sticky Symbols pane.
  // Resizable owns widths; it must not introduce a nested vertical scrollport.
  return (
    <ResizablePanelGroup
      elementRef={setWorkspace}
      orientation="horizontal"
      className="file-source-workspace"
      style={{ overflow: "visible" }}
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
