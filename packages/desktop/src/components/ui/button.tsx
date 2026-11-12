import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";
import * as React from "react";
import { ControlTooltip } from "@/components/ui/tooltip";

import { cn } from "@/lib/utils";

function hasText(children: React.ReactNode): boolean {
  return React.Children.toArray(children).some((child) =>
    typeof child === "string"
      ? Boolean(child.trim())
      : typeof child === "number" ||
        (React.isValidElement<{ children?: React.ReactNode }>(child) &&
          hasText(child.props.children)),
  );
}

const buttonVariants = cva(
  "inline-flex items-center justify-center gap-2 border border-transparent whitespace-nowrap rounded-md text-sm font-medium transition-colors focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground hover:bg-primary-hover",
        destructive:
          "bg-destructive text-destructive-foreground hover:bg-destructive/90",
        outline:
          "border-input bg-card text-foreground hover:bg-accent hover:text-accent-foreground",
        secondary:
          "bg-secondary text-secondary-foreground hover:bg-secondary/80",
        ghost: "hover:bg-accent hover:text-accent-foreground",
        link: "text-primary underline-offset-4 hover:underline",
      },
      size: {
        default: "min-h-8 px-3 py-1.5",
        compact: "px-2 py-1 text-xs whitespace-normal",
        sm: "h-8 rounded-md px-3 text-xs",
        lg: "h-10 rounded-md px-8",
        icon: "h-6 w-6 shrink-0 p-0 text-muted-foreground",
      },
    },
    defaultVariants: {
      variant: "outline",
      size: "default",
    },
  },
);

export interface ButtonProps
  extends
    React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean;
}

const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, title, asChild = false, ...props }, ref) => {
    const Comp = asChild ? Slot : "button";
    return (
      <ControlTooltip title={props["aria-haspopup"] ? undefined : title}>
        <Comp
        className={cn(buttonVariants({ variant, size, className }))}
        ref={ref}
        {...props}
          aria-label={
            props["aria-label"] ??
            (!hasText(props.children) ? title : undefined)
          }
        />
      </ControlTooltip>
    );
  },
);
Button.displayName = "Button";

export { Button, buttonVariants };
