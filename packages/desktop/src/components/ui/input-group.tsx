import * as React from "react";

import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

const InputGroup = React.forwardRef<HTMLDivElement, React.ComponentProps<"div">>(
  ({ className, ...props }, ref) => (
    <div
      ref={ref}
      role="group"
      data-slot="input-group"
      className={cn(
        "relative flex h-8 min-w-0 w-full items-center rounded-md border border-input bg-background transition-colors focus-within:border-ring focus-within:ring-1 focus-within:ring-ring",
        className,
      )}
      {...props}
    />
  ),
);
InputGroup.displayName = "InputGroup";

function InputGroupAddon({
  className,
  align = "inline-start",
  onClick,
  ...props
}: React.ComponentProps<"div"> & {
  align?: "inline-start" | "inline-end" | "block-start" | "block-end";
}) {
  return (
    <div
      role="group"
      data-slot="input-group-addon"
      data-align={align}
      className={cn(
        "flex shrink-0 items-center gap-0.5 text-muted-foreground",
        align === "inline-start" && "order-first pl-1",
        align === "inline-end" && "order-last pr-1",
        align === "block-start" && "order-first w-full px-3 pt-2",
        align === "block-end" && "order-last w-full px-3 pb-2",
        className,
      )}
      onClick={(event) => {
        onClick?.(event);
        // Portalled menus keep their own focus scope outside the addon.
        if (event.defaultPrevented || !event.currentTarget.contains(event.target as Node) || (event.target as HTMLElement).closest("button")) return;
        // Hidden attachment inputs are not the composer editor.
        event.currentTarget.parentElement?.querySelector<HTMLElement>('[data-slot="input-group-control"]')?.focus();
      }}
      {...props}
    />
  );
}

const InputGroupInput = React.forwardRef<
  HTMLInputElement,
  React.ComponentPropsWithoutRef<typeof Input>
>(({ className, ...props }, ref) => (
  <Input
    ref={ref}
    data-slot="input-group-control"
    className={cn(
      "h-full flex-1 rounded-none border-0 bg-transparent shadow-none focus-visible:ring-0",
      className,
    )}
    {...props}
  />
));
InputGroupInput.displayName = "InputGroupInput";

const InputGroupTextarea = React.forwardRef<
  HTMLTextAreaElement,
  React.ComponentPropsWithoutRef<typeof Textarea>
>(({ className, ...props }, ref) => (
  <Textarea
    ref={ref}
    data-slot="input-group-control"
    className={cn(
      // Auto-growing composers set height; an automatic basis preserves it.
      "flex-auto resize-none rounded-none border-0 bg-transparent shadow-none focus-visible:ring-0",
      className,
    )}
    {...props}
  />
));
InputGroupTextarea.displayName = "InputGroupTextarea";

export { InputGroup, InputGroupAddon, InputGroupInput, InputGroupTextarea };
