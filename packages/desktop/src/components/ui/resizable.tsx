import type { ComponentProps } from "react";
import * as ResizablePrimitive from "react-resizable-panels";
import { cn } from "@/lib/utils";

// shadcn/ui's composition for the installed react-resizable-panels v2 API.
// Keep the primitive's keyboard and collapse semantics on both split axes.
export function ResizablePanelGroup({ className, ...props }: ComponentProps<typeof ResizablePrimitive.PanelGroup>) {
  return <ResizablePrimitive.PanelGroup className={cn("flex h-full w-full data-[panel-group-direction=vertical]:flex-col", className)} {...props} />;
}

export const ResizablePanel = ResizablePrimitive.Panel;

// A visible hairline marks the boundary; the wider hit target remains easy to drag.
export function ResizableHandle({ className, ...props }: ComponentProps<typeof ResizablePrimitive.PanelResizeHandle>) {
  return (
    <ResizablePrimitive.PanelResizeHandle
      aria-orientation="vertical"
      className={cn(
        "relative flex w-1 shrink-0 items-center justify-center bg-transparent after:absolute after:inset-y-0 after:left-1/2 after:w-px after:-translate-x-1/2 after:bg-muted-foreground/75 after:transition-colors hover:after:bg-primary data-[resize-handle-state=drag]:after:bg-primary focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring data-[panel-group-direction=vertical]:h-1 data-[panel-group-direction=vertical]:w-full data-[panel-group-direction=vertical]:after:inset-x-0 data-[panel-group-direction=vertical]:after:top-1/2 data-[panel-group-direction=vertical]:after:h-px data-[panel-group-direction=vertical]:after:w-full data-[panel-group-direction=vertical]:after:-translate-y-1/2 data-[panel-group-direction=vertical]:after:translate-x-0",
        className,
      )}
      {...props}
    />
  );
}
