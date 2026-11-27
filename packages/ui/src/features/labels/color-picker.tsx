import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { CheckIcon } from "lucide-react";
import {
  useEffect,
  useId,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent,
} from "react";
import {
  PRESET_LABEL_COLORS,
  contrastForeground,
  hexToHsv,
  hsvToHex,
  isValidHexColor,
  normalizeHexInput,
  type HsvColor,
} from "./color";

const DEFAULT_LABEL_COLOR = "0969da";

function clampUnit(value: number): number {
  return Math.min(1, Math.max(0, value));
}

function clampHue(value: number): number {
  return Math.min(360, Math.max(0, value));
}

export function LabelColorPicker({
  color,
  onChange,
}: {
  color: string;
  onChange: (color: string) => void;
}) {
  const inputId = useId();
  const normalized = normalizeHexInput(color);
  const activeHex = isValidHexColor(normalized)
    ? normalized
    : DEFAULT_LABEL_COLOR;
  const [hsv, setHsv] = useState<HsvColor>(() => hexToHsv(activeHex));
  const lastEmittedRef = useRef<string>(activeHex);

  useEffect(() => {
    if (isValidHexColor(normalized) && normalized !== lastEmittedRef.current) {
      lastEmittedRef.current = normalized;
      setHsv(hexToHsv(normalized));
    }
  }, [normalized]);

  function commitHsv(nextHsv: HsvColor, explicitHex?: string) {
    const nextHex = explicitHex ?? hsvToHex(nextHsv);
    lastEmittedRef.current = nextHex;
    setHsv(nextHsv);
    onChange(nextHex);
  }

  function updateSaturationBrightness(
    clientX: number,
    clientY: number,
    rect: DOMRect,
  ) {
    if (rect.width <= 0 || rect.height <= 0) return;
    const s = clampUnit((clientX - rect.left) / rect.width);
    const v = clampUnit(1 - (clientY - rect.top) / rect.height);
    commitHsv({ h: hsv.h, s, v });
  }

  function updateHue(clientX: number, rect: DOMRect) {
    if (rect.width <= 0) return;
    const ratio = clampUnit((clientX - rect.left) / rect.width);
    const h = clampHue(ratio * 360);
    commitHsv({ ...hsv, h });
  }

  function handleSaturationPointerDown(event: PointerEvent<HTMLDivElement>) {
    event.preventDefault();
    event.currentTarget.focus();
    event.currentTarget.setPointerCapture(event.pointerId);
    updateSaturationBrightness(
      event.clientX,
      event.clientY,
      event.currentTarget.getBoundingClientRect(),
    );
  }

  function handleSaturationPointerMove(event: PointerEvent<HTMLDivElement>) {
    if (!event.currentTarget.hasPointerCapture(event.pointerId)) return;
    updateSaturationBrightness(
      event.clientX,
      event.clientY,
      event.currentTarget.getBoundingClientRect(),
    );
  }

  function handleHuePointerDown(event: PointerEvent<HTMLDivElement>) {
    event.preventDefault();
    event.currentTarget.focus();
    event.currentTarget.setPointerCapture(event.pointerId);
    updateHue(event.clientX, event.currentTarget.getBoundingClientRect());
  }

  function handleHuePointerMove(event: PointerEvent<HTMLDivElement>) {
    if (!event.currentTarget.hasPointerCapture(event.pointerId)) return;
    updateHue(event.clientX, event.currentTarget.getBoundingClientRect());
  }

  function handleSaturationKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const step = event.shiftKey ? 0.15 : 0.05;
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      commitHsv({ ...hsv, s: clampUnit(hsv.s - step) });
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      commitHsv({ ...hsv, s: clampUnit(hsv.s + step) });
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      commitHsv({ ...hsv, v: clampUnit(hsv.v + step) });
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      commitHsv({ ...hsv, v: clampUnit(hsv.v - step) });
    }
  }

  function handleHueKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const step = event.shiftKey ? 15 : 5;
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      commitHsv({ ...hsv, h: clampHue(hsv.h - step) });
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      commitHsv({ ...hsv, h: clampHue(hsv.h + step) });
    }
  }

  const saturationPercent = Math.round(hsv.s * 100);
  const brightnessPercent = Math.round(hsv.v * 100);
  const hueDegrees = Math.round(hsv.h);

  return (
    <div className="label-color-field">
      <label htmlFor={inputId}>Color</label>
      <div className="flex items-center gap-2">
        <Popover>
          <PopoverTrigger asChild>
            <Button
              type="button"
              variant="outline"
              aria-label="Choose label color"
              className="size-8 shrink-0 rounded-md border border-black/15 p-0 shadow-xs dark:border-white/20"
              style={{ backgroundColor: `#${activeHex}` }}
            />
          </PopoverTrigger>
          <PopoverContent
            align="start"
            sideOffset={6}
            className="w-[264px] max-w-[calc(100vw-2rem)] gap-3 p-3"
            aria-label="Label color picker"
          >
            <div
              role="slider"
              tabIndex={0}
              aria-label="Color saturation and brightness"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={saturationPercent}
              aria-valuetext={`Saturation ${saturationPercent}%, brightness ${brightnessPercent}%`}
              className="relative h-36 w-full cursor-crosshair touch-none select-none overflow-hidden rounded-md border border-black/15 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring dark:border-white/15"
              style={{ backgroundColor: `hsl(${hsv.h} 100% 50%)` }}
              onPointerDown={handleSaturationPointerDown}
              onPointerMove={handleSaturationPointerMove}
              onKeyDown={handleSaturationKeyDown}
            >
              <div
                aria-hidden="true"
                className="pointer-events-none absolute inset-0"
                style={{
                  background: "linear-gradient(to right, #ffffff, transparent)",
                }}
              />
              <div
                aria-hidden="true"
                className="pointer-events-none absolute inset-0"
                style={{
                  background: "linear-gradient(to top, #000000, transparent)",
                }}
              />
              <span
                aria-hidden="true"
                className="pointer-events-none absolute size-4 -translate-x-1/2 -translate-y-1/2 rounded-full border-2 border-white shadow-[0_0_0_1px_rgba(0,0,0,0.35),0_1px_3px_rgba(0,0,0,0.45)]"
                style={{
                  left: `${hsv.s * 100}%`,
                  top: `${(1 - hsv.v) * 100}%`,
                  backgroundColor: `#${activeHex}`,
                }}
              />
            </div>

            <div
              role="slider"
              tabIndex={0}
              aria-label="Color hue"
              aria-valuemin={0}
              aria-valuemax={360}
              aria-valuenow={hueDegrees}
              aria-valuetext={`${hueDegrees}°`}
              className="relative h-4 w-full cursor-ew-resize touch-none select-none rounded-full border border-black/15 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring dark:border-white/15"
              style={{
                background:
                  "linear-gradient(to right, #ff0000 0%, #ffff00 17%, #00ff00 33%, #00ffff 50%, #0000ff 67%, #ff00ff 83%, #ff0000 100%)",
              }}
              onPointerDown={handleHuePointerDown}
              onPointerMove={handleHuePointerMove}
              onKeyDown={handleHueKeyDown}
            >
              <span
                aria-hidden="true"
                className="pointer-events-none absolute top-1/2 size-4 -translate-x-1/2 -translate-y-1/2 rounded-full border-2 border-white shadow-[0_0_0_1px_rgba(0,0,0,0.35),0_1px_3px_rgba(0,0,0,0.45)]"
                style={{
                  left: `${(hsv.h / 360) * 100}%`,
                  backgroundColor: `hsl(${hsv.h} 100% 50%)`,
                }}
              />
            </div>

            <div
              className="grid grid-cols-7 gap-1.5"
              role="group"
              aria-label="Predefined colors"
            >
              {PRESET_LABEL_COLORS.map((preset) => {
                const selected = normalized === preset.hex;
                return (
                  <button
                    key={preset.hex}
                    type="button"
                    aria-label={`${preset.name} (#${preset.hex})`}
                    aria-pressed={selected}
                    className="flex aspect-square min-h-7 w-full items-center justify-center rounded-md border border-black/15 transition-transform hover:scale-105 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 dark:border-white/15"
                    style={{ backgroundColor: `#${preset.hex}` }}
                    onClick={() => commitHsv(hexToHsv(preset.hex), preset.hex)}
                  >
                    {selected && (
                      <CheckIcon
                        aria-hidden="true"
                        className="size-4 stroke-[2.5]"
                        style={{ color: contrastForeground(preset.hex) }}
                      />
                    )}
                  </button>
                );
              })}
            </div>
          </PopoverContent>
        </Popover>
        <Input
          id={inputId}
          className="w-24 font-mono"
          required
          pattern="[0-9a-fA-F]{6}"
          title="Enter a six-digit hex color"
          maxLength={7}
          value={color}
          onChange={(event) =>
            onChange(normalizeHexInput(event.target.value).slice(0, 6))
          }
        />
      </div>
    </div>
  );
}
