import type { ComponentProps } from "react";
import * as ResizablePrimitive from "react-resizable-panels";
import clsx from "clsx";

// shadcn/ui's composition for the installed react-resizable-panels v2 API.
// Keep the primitive's keyboard and collapse semantics on both split axes.
export function ResizablePanelGroup({ className, ...props }: ComponentProps<typeof ResizablePrimitive.PanelGroup>) {
  return <ResizablePrimitive.PanelGroup className={clsx("flex h-full w-full data-[panel-group-direction=vertical]:flex-col", className)} {...props} />;
}

export const ResizablePanel = ResizablePrimitive.Panel;

export function ResizableHandle({ className, ...props }: ComponentProps<typeof ResizablePrimitive.PanelResizeHandle>) {
  return (
    <ResizablePrimitive.PanelResizeHandle
      className={clsx(
        "relative flex w-px shrink-0 items-center justify-center bg-gx-border after:absolute after:inset-y-0 after:left-1/2 after:w-1 after:-translate-x-1/2 hover:bg-gx-accent focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-gx-accent data-[panel-group-direction=vertical]:h-px data-[panel-group-direction=vertical]:w-full data-[panel-group-direction=vertical]:after:left-0 data-[panel-group-direction=vertical]:after:h-1 data-[panel-group-direction=vertical]:after:w-full data-[panel-group-direction=vertical]:after:-translate-y-1/2 data-[panel-group-direction=vertical]:after:translate-x-0",
        className,
      )}
      {...props}
    />
  );
}
