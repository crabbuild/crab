/**
 * Resize Handle — Draggable divider for resizable panels.
 *
 * Renders a thin vertical or horizontal bar that can be dragged to
 * resize adjacent panels. Provides visual feedback on hover/drag.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import clsx from "clsx";

interface Props {
  /** Direction of resize: "horizontal" resizes width, "vertical" resizes height. */
  direction?: "horizontal" | "vertical";
  /** Callback with the delta in pixels during drag. */
  onResize: (delta: number) => void;
  /** Optional class name for the handle container. */
  className?: string;
}

export function ResizeHandle({ direction = "horizontal", onResize, className }: Props) {
  const [dragging, setDragging] = useState(false);
  const startPosRef = useRef(0);

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    setDragging(true);
    startPosRef.current = direction === "horizontal" ? e.clientX : e.clientY;
  }, [direction]);

  useEffect(() => {
    if (!dragging) return;

    const handleMouseMove = (e: MouseEvent) => {
      const currentPos = direction === "horizontal" ? e.clientX : e.clientY;
      const delta = currentPos - startPosRef.current;
      if (delta !== 0) {
        onResize(delta);
        startPosRef.current = currentPos;
      }
    };

    const handleMouseUp = () => {
      setDragging(false);
    };

    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);

    // Prevent text selection during drag.
    document.body.style.userSelect = "none";
    document.body.style.cursor = direction === "horizontal" ? "col-resize" : "row-resize";

    return () => {
      document.removeEventListener("mousemove", handleMouseMove);
      document.removeEventListener("mouseup", handleMouseUp);
      document.body.style.userSelect = "";
      document.body.style.cursor = "";
    };
  }, [dragging, direction, onResize]);

  const isHorizontal = direction === "horizontal";

  return (
    <div
      onMouseDown={handleMouseDown}
      className={clsx(
        "group relative flex items-center justify-center transition-colors",
        isHorizontal
          ? "w-1 cursor-col-resize hover:bg-primary/20"
          : "h-1 cursor-row-resize hover:bg-primary/20",
        dragging && "bg-primary/30",
        className,
      )}
      role="separator"
      aria-orientation={isHorizontal ? "vertical" : "horizontal"}
    >
      {/* Visual indicator line */}
      <div
        className={clsx(
          "bg-muted-foreground/75 transition-colors group-hover:bg-primary",
          isHorizontal ? "h-full w-px" : "h-px w-full",
          dragging && "bg-primary",
        )}
      />
    </div>
  );
}
