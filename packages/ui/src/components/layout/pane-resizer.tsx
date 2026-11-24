import { Button } from "@/components/ui/button";
import { useRef, type KeyboardEvent, type PointerEvent } from "react";

type Props = {
  className: string;
  label: string;
  controls: string;
  value: number;
  min: number;
  max: number;
  defaultValue: number;
  step: number;
  unit: "percent" | "pixels";
  valueText: string;
  onChange: (value: number) => void;
};

function clamp(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, Math.round(value)));
}

export function PaneResizer({
  className,
  label,
  controls,
  value,
  min,
  max,
  defaultValue,
  step,
  unit,
  valueText,
  onChange,
}: Props) {
  const drag = useRef<{ pointerId: number; x: number; value: number } | null>(
    null,
  );
  const update = (next: number) => onChange(clamp(next, min, max));
  return (
    <Button
      variant="ghost"
      type="button"
      className={`pane-resizer ${className}`}
      role="separator"
      aria-label={label}
      aria-controls={controls}
      aria-orientation="vertical"
      aria-valuemin={min}
      aria-valuemax={max}
      aria-valuenow={value}
      aria-valuetext={valueText}
      title="Drag or use arrow keys to resize. Double-click to reset."
      onDoubleClick={() => update(defaultValue)}
      onKeyDown={(event: KeyboardEvent<HTMLButtonElement>) => {
        let next: number | undefined;
        if (event.key === "ArrowLeft") next = value - step;
        if (event.key === "ArrowRight") next = value + step;
        if (event.key === "Home") next = min;
        if (event.key === "End") next = max;
        if (next === undefined) return;
        event.preventDefault();
        update(next);
      }}
      onPointerDown={(event: PointerEvent<HTMLButtonElement>) => {
        if (drag.current || event.button !== 0 || !event.isPrimary) return;
        event.preventDefault();
        // Preserve the grab offset so widening the target cannot make the pane jump.
        drag.current = { pointerId: event.pointerId, x: event.clientX, value };
        event.currentTarget.setPointerCapture(event.pointerId);
      }}
      onPointerMove={(event: PointerEvent<HTMLButtonElement>) => {
        if (
          drag.current?.pointerId !== event.pointerId ||
          !event.currentTarget.hasPointerCapture(event.pointerId)
        )
          return;
        const container = event.currentTarget.parentElement;
        if (!container) return;
        event.preventDefault();
        const bounds = container.getBoundingClientRect();
        const delta = event.clientX - drag.current.x;
        const distance =
          unit === "percent" ? (delta / bounds.width) * 100 : delta;
        update(drag.current.value + distance);
      }}
      onPointerUp={(event: PointerEvent<HTMLButtonElement>) => {
        if (
          drag.current?.pointerId === event.pointerId &&
          event.currentTarget.hasPointerCapture(event.pointerId)
        )
          event.currentTarget.releasePointerCapture(event.pointerId);
      }}
      onLostPointerCapture={(event: PointerEvent<HTMLButtonElement>) => {
        if (drag.current?.pointerId === event.pointerId) drag.current = null;
      }}
    >
      {/* The tree limits hits to this target to keep its scrollbar reachable.
          Events still bubble to the separator, which owns pointer capture. */}
      <span className="pane-resizer-target" aria-hidden="true" />
    </Button>
  );
}
