import { useEffect, useState, type ReactNode } from "react";
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup,
} from "@/components/ui/resizable";

export function SourceWorkspace({
  children,
  symbols,
}: {
  children: ReactNode;
  symbols: ReactNode;
}) {
  const [stacked, setStacked] = useState(
    () => matchMedia("(max-width: 900px)").matches,
  );
  useEffect(() => {
    const media = matchMedia("(max-width: 900px)");
    const update = () => setStacked(media.matches);
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);

  if (!symbols || stacked)
    return (
      <div className="file-source-workspace">
        {children}
        {symbols}
      </div>
    );

  // Visible overflow preserves document scrolling and the sticky Symbols pane.
  // Resizable owns widths; it must not introduce a nested vertical scrollport.
  return (
    <ResizablePanelGroup
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
        defaultSize="30%"
        minSize={200}
        maxSize="60%"
        style={{ overflow: "visible" }}
      >
        {symbols}
      </ResizablePanel>
    </ResizablePanelGroup>
  );
}
