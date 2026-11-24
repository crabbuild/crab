import type { ComponentProps } from "react";
import { Slider as SliderPrimitive } from "radix-ui";
import { cn } from "@/lib/utils";

export function Slider({
  className,
  label,
  value,
  onValueChange,
  ...props
}: Omit<
  ComponentProps<typeof SliderPrimitive.Root>,
  "value" | "defaultValue" | "onValueChange"
> & {
  label: string;
  value: number;
  onValueChange: (value: number) => void;
}) {
  return (
    <SliderPrimitive.Root
      data-slot="slider"
      className={cn(
        "relative flex h-6 w-full touch-none items-center select-none data-disabled:opacity-50",
        className,
      )}
      value={[value]}
      onValueChange={([value]) => onValueChange(value)}
      {...props}
    >
      <SliderPrimitive.Track className="relative h-1 w-full overflow-hidden rounded-full bg-muted">
        <SliderPrimitive.Range className="absolute h-full bg-primary" />
      </SliderPrimitive.Track>
      <SliderPrimitive.Thumb
        aria-label={label}
        data-slot="slider-thumb"
        className="block size-3 rounded-full border border-ring bg-background ring-ring/50 after:absolute after:-inset-2 hover:ring-3 focus-visible:ring-3 focus-visible:outline-hidden"
      />
    </SliderPrimitive.Root>
  );
}
