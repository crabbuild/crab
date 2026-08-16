"use client";

import { useRef, type HTMLAttributes, type ReactNode } from "react";
import { cn } from "@/lib/utils";
import { CopyButton } from "./copy-button";

interface DocsCodeBlockProps extends HTMLAttributes<HTMLElement> {
  children?: ReactNode;
  title?: string;
  icon?: ReactNode;
}

/**
 * Custom CodeBlock that wraps code content with a copy-to-clipboard button.
 * Handles clipboard API failures by showing an error state.
 * Replaces the default Fumadocs CodeBlock to satisfy requirement 10.2/10.3.
 */
export function DocsCodeBlock({
  children,
  title,
  icon,
  className,
  ...props
}: DocsCodeBlockProps) {
  const areaRef = useRef<HTMLDivElement>(null);

  return (
    <figure
      dir="ltr"
      tabIndex={-1}
      className={cn(
        "my-4 bg-fd-card rounded-xl shiki relative border shadow-sm not-prose overflow-hidden text-sm",
        "bg-(--shiki-light-bg) dark:bg-(--shiki-dark-bg)",
        className
      )}
      {...props}
    >
      {title ? (
        <div className="flex text-fd-muted-foreground items-center gap-2 h-9.5 border-b px-4">
          {icon && (
            <div className="[&_svg]:size-3.5">
              {typeof icon === "string" ? (
                <span dangerouslySetInnerHTML={{ __html: icon }} />
              ) : (
                icon
              )}
            </div>
          )}
          <figcaption className="flex-1 truncate">{title}</figcaption>
          <div className="-me-2 empty:hidden">
            <CopyButton containerRef={areaRef} />
          </div>
        </div>
      ) : (
        <div className="absolute top-3 right-2 z-2 backdrop-blur-lg rounded-lg text-fd-muted-foreground empty:hidden">
          <CopyButton containerRef={areaRef} />
        </div>
      )}
      <div
        ref={areaRef}
        role="region"
        tabIndex={0}
        className="text-[0.8125rem] py-3.5 overflow-auto max-h-[600px] fd-scroll-container focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-fd-ring"
      >
        {children}
      </div>
    </figure>
  );
}
