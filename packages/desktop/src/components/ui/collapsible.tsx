import { cn } from "@/lib/utils";
import * as CollapsiblePrimitive from "@radix-ui/react-collapsible";
import { ChevronRight } from "lucide-react";
import * as React from "react";

const Collapsible = CollapsiblePrimitive.Root;
const CollapsibleContent = CollapsiblePrimitive.Content;
const CollapsibleTrigger = React.forwardRef<
  React.ElementRef<typeof CollapsiblePrimitive.Trigger>,
  React.ComponentPropsWithoutRef<typeof CollapsiblePrimitive.Trigger>
>(({ className, children, ...props }, ref) => (
  <CollapsiblePrimitive.Trigger
    ref={ref}
    className={cn(
      "group inline-flex items-center gap-1 text-left text-xs text-muted-foreground hover:text-foreground",
      className,
    )}
    {...props}
  >
    <ChevronRight className="size-3 shrink-0 transition-transform group-data-[state=open]:rotate-90" />
    {children}
  </CollapsiblePrimitive.Trigger>
));
CollapsibleTrigger.displayName = "CollapsibleTrigger";
export { Collapsible, CollapsibleContent, CollapsibleTrigger };
