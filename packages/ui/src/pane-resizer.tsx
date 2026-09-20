import type { KeyboardEvent, PointerEvent } from "react";

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
  const update = (next: number) => onChange(clamp(next, min, max));
  return (
    <button
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
      title="Drag or use arrow keys to resize"
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
        event.preventDefault();
        event.currentTarget.setPointerCapture(event.pointerId);
      }}
      onPointerMove={(event: PointerEvent<HTMLButtonElement>) => {
        if (!event.currentTarget.hasPointerCapture(event.pointerId)) return;
        const container = event.currentTarget.parentElement;
        if (!container) return;
        event.preventDefault();
        const bounds = container.getBoundingClientRect();
        const offset = event.clientX - bounds.left;
        update(unit === "percent" ? (offset / bounds.width) * 100 : offset);
      }}
      onPointerUp={(event: PointerEvent<HTMLButtonElement>) => {
        event.currentTarget.releasePointerCapture(event.pointerId);
      }}
    />
  );
}
