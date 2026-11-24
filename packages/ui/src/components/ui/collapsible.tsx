"use client";

import { Collapsible as CollapsiblePrimitive } from "radix-ui";
import { ChevronRightIcon } from "lucide-react";
import { useState, type ReactNode } from "react";
import { cn } from "@/lib/utils";

function Collapsible({
  ...props
}: React.ComponentProps<typeof CollapsiblePrimitive.Root>) {
  return <CollapsiblePrimitive.Root data-slot="collapsible" {...props} />;
}

function CollapsibleTrigger({
  ...props
}: React.ComponentProps<typeof CollapsiblePrimitive.CollapsibleTrigger>) {
  return (
    <CollapsiblePrimitive.CollapsibleTrigger
      data-slot="collapsible-trigger"
      {...props}
    />
  );
}

function CollapsibleContent({
  ...props
}: React.ComponentProps<typeof CollapsiblePrimitive.CollapsibleContent>) {
  return (
    <CollapsiblePrimitive.CollapsibleContent
      data-slot="collapsible-content"
      {...props}
    />
  );
}

function Disclosure({
  expanded = false,
  summary,
  summaryClassName,
  summaryLabel,
  indicator = true,
  onOpenChange,
  children,
  ...props
}: Omit<React.ComponentProps<typeof Collapsible>, "open" | "defaultOpen"> & {
  expanded?: boolean;
  summary: ReactNode;
  summaryClassName?: string;
  summaryLabel?: string;
  indicator?: boolean;
}) {
  const [state, setState] = useState({ expanded, open: expanded });
  if (state.expanded !== expanded) {
    setState({ expanded, open: expanded });
  }
  return (
    <Collapsible
      {...props}
      open={state.expanded === expanded ? state.open : expanded}
      onOpenChange={(open) => {
        setState({ expanded, open });
        onOpenChange?.(open);
      }}
    >
      <CollapsibleTrigger
        aria-label={summaryLabel}
        className={cn(
          "group/disclosure inline-flex items-center gap-1.5 rounded-sm text-start focus-visible:ring-2 focus-visible:ring-ring/25 focus-visible:outline-none",
          summaryClassName,
        )}
      >
        {indicator && (
          <ChevronRightIcon
            aria-hidden="true"
            className="size-4 shrink-0 text-muted-foreground group-data-[state=open]/disclosure:rotate-90"
          />
        )}
        {summary}
      </CollapsibleTrigger>
      <CollapsibleContent
        forceMount
        className="contents data-[state=closed]:hidden"
      >
        {children}
      </CollapsibleContent>
    </Collapsible>
  );
}

export { Collapsible, CollapsibleTrigger, CollapsibleContent, Disclosure };
