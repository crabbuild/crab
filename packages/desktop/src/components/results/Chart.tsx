import { useEffect, useRef, useState } from "react";
import type {
  Chart as ChartData,
  AxisRange,
  ChartInteraction,
  ChartImage,
  EvidenceView,
  ResultsResponse,
} from "../../lib/results-contracts";
import {
  render,
  validRange,
  zoomRange,
  type ChartView,
} from "../../../../../crates/crab-workflow/assets/results-runtime.mjs";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ChevronDown, Link2, RotateCcw, ZoomIn, ZoomOut } from "lucide-react";

export function ResultChart({
  chart,
  cursor,
  onCursor,
  loadImage,
  range,
  resetRange,
  immutable,
  onRange,
  onReset,
}: {
  chart: ChartData;
  immutable: boolean;
  range: AxisRange | null;
  resetRange: AxisRange | null;
  onRange: (key: string, range: AxisRange | null) => void;
  onReset: (key: string) => void;
  loadImage: (image: ChartImage) => {
    promise: Promise<ResultsResponse>;
    cancel: () => void;
  };
  cursor: { key: string; field: string; value: unknown } | null;
  onCursor: (value: { key: string; field: string; value: unknown }) => void;
}) {
  const liveView = useRef<ChartView | undefined>(undefined);
  const notify = useRef(onCursor);
  notify.current = onCursor;
  const latest = useRef({ range, cursor });
  latest.current = { range, cursor };
  const axis = chart.interaction;
  const container = useRef<HTMLDivElement>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    let stopped = false;
    let view: ChartView | undefined;
    let dispose: (() => void) | undefined;
    setError("");
    container.current?.replaceChildren();
    if (chart.spec && container.current) {
      // A late renderer owns only this generation's host. Refresh can detach
      // it without letting old evidence reappear when the new spec is absent.
      const host = document.createElement("div");
      container.current.append(host);
      void render(host, chart.spec, {
        interaction: axis,
        onError: (error) => {
          if (!stopped) {
            liveView.current = undefined;
            setError(String(error));
          }
        },
      })
        .then(async (result) => {
          if (stopped) result.finalize();
          else {
            dispose = result.finalize;
            view = result.view;
            liveView.current = view;
            if (axis) {
              view.addSignalListener("crab_cursor", (_name, value) => {
                notify.current({
                  key: axis.key,
                  field: axis.field,
                  value: value ?? null,
                });
              });
              const state = latest.current;
              view.signal(
                "crab_shared_x",
                state.cursor?.key === axis.key ? state.cursor.value : null,
              );
              if (axis.domain)
                view.signal(
                  "crab_domain",
                  state.range ? [state.range.lower, state.range.upper] : null,
                );
              await view.runAsync();
            }
          }
        })
        .catch((error) => {
          if (!stopped) setError(String(error));
        });
    }
    return () => {
      stopped = true;
      dispose?.();
      liveView.current = undefined;
    };
  }, [chart.spec, axis]);
  useEffect(() => {
    if (axis && liveView.current) {
      void liveView.current
        .signal("crab_shared_x", cursor?.key === axis.key ? cursor.value : null)
        .runAsync()
        .catch((error) => setError(String(error)));
    }
  }, [cursor, axis]);
  useEffect(() => {
    if (axis?.domain && liveView.current) {
      void liveView.current
        .signal("crab_domain", range ? [range.lower, range.upper] : null)
        .runAsync()
        .catch((error) => setError(String(error)));
    }
  }, [range, axis]);
  return (
    <section
      className="min-w-0 border-t border-border py-6"
      aria-label={chart.title}
      data-axis-key={axis?.key}
    >
      <h3 className="mb-3 truncate text-sm font-medium" title={chart.title}>
        {chart.title}
      </h3>
      {axis?.domain && !error && (
        <AxisControls
          key={axis.key}
          axis={axis}
          immutable={immutable}
          range={range}
          resetRange={resetRange}
          title={chart.title}
          onChange={(value) => onRange(axis.key, value)}
          onReset={() => onReset(axis.key)}
        />
      )}
      {chart.reason && (
        <p className="py-1 text-xs text-muted-foreground">{chart.reason}</p>
      )}
      {chart.reduction === "counted" && (
        <p className="text-xs text-muted-foreground">
          Counted all {chart.points} retained observations into{" "}
          {chart.displayed_points} observed class pairs. Missing class
          combinations are shown as zero.
        </p>
      )}
      {chart.reduction === "sampled" && (
        <p className="text-xs text-muted-foreground">
          Showing {chart.displayed_points} sampled points of {chart.points}.
          Source bytes remain retained.
        </p>
      )}
      {error && (
        <p role="alert" className="break-words text-destructive">
          {error}
        </p>
      )}
      {cursor && cursor.value !== null && cursor.key === axis?.key && (
        <p className="text-xs text-muted-foreground" aria-live="polite">
          Cursor {cursor.field}: {String(cursor.value)}
        </p>
      )}
      {chart.images.length > 0 && (
        <div className="grid gap-4 sm:grid-cols-2">
          {chart.images.map((image) => (
            <ImagePreview
              key={`${image.digest}:${image.path}`}
              image={image}
              loadImage={loadImage}
            />
          ))}
        </div>
      )}
      <div
        ref={container}
        tabIndex={0}
        className="mt-3 overflow-auto rounded-sm focus-visible:outline focus-visible:outline-2 focus-visible:outline-ring"
      />
    </section>
  );
}

function AxisControls({
  axis,
  immutable,
  range,
  resetRange,
  title,
  onChange,
  onReset,
}: {
  axis: ChartInteraction;
  immutable: boolean;
  range: AxisRange | null;
  resetRange: AxisRange | null;
  title: string;
  onChange: (range: AxisRange | null) => void;
  onReset: () => void;
}) {
  const [draft, setDraft] = useState({
    base: range,
    lower: range ? String(range.lower) : "",
    upper: range ? String(range.upper) : "",
  });
  // New bounds replace displayed values without replacing the focused form.
  // Tag edits with their committed range so comparison refreshes preserve drafts.
  const text =
    draft.base?.lower === range?.lower && draft.base?.upper === range?.upper
      ? draft
      : {
          base: range,
          lower: range ? String(range.lower) : "",
          upper: range ? String(range.upper) : "",
        };
  if (text !== draft) setDraft(text);
  const domain = axis.domain;
  if (!domain) return null;
  const bounds = {
    lower: text.lower.trim() ? Number(text.lower) : domain.lower,
    upper: text.upper.trim() ? Number(text.upper) : domain.upper,
  };
  return (
    <form
      className="mb-3 flex flex-wrap items-end gap-2 text-xs"
      aria-label={`X range ${title}`}
      onSubmit={(event) => {
        event.preventDefault();
        if (validRange(domain, bounds)) onChange(bounds);
      }}
    >
      <details className="group basis-full text-[11px] text-muted-foreground">
        <summary className="flex w-fit cursor-pointer items-center gap-1.5 rounded-sm focus-visible:outline focus-visible:outline-ring">
          <Link2 size={12} aria-hidden="true" /> Linked x axis · {axis.label}
          <span className="font-mono">
            {domain.lower}–{domain.upper}
          </span>
          <ChevronDown
            size={12}
            className="transition-transform group-open:rotate-180"
            aria-hidden="true"
          />
        </summary>
        <p className="mt-1 max-w-prose">
          {immutable
            ? "Report inspection is temporary. Reset restores the saved range."
            : "Ranges are saved with your working comparison."}
        </p>
      </details>
      <label className="min-w-0 text-muted-foreground">
        From
        <Input
          aria-label={`X minimum ${title}`}
          type="number"
          step="any"
          className="mt-1 h-8 w-20 font-mono text-xs"
          placeholder="Auto"
          value={text.lower}
          onChange={(event) => setDraft({ ...text, lower: event.target.value })}
        />
      </label>
      <label className="min-w-0 text-muted-foreground">
        To
        <Input
          aria-label={`X maximum ${title}`}
          type="number"
          step="any"
          className="mt-1 h-8 w-20 font-mono text-xs"
          placeholder="Auto"
          value={text.upper}
          onChange={(event) => setDraft({ ...text, upper: event.target.value })}
        />
      </label>
      <Button
        type="submit"
        size="sm"
        variant="outline"
        disabled={!validRange(domain, bounds)}
      >
        Apply range
      </Button>
      <Button
        type="button"
        size="icon-sm"
        variant="ghost"
        aria-label="Zoom in"
        title="Zoom in"
        onClick={() => onChange(zoomRange(domain, range, 0.5))}
      >
        <ZoomIn size={14} />
      </Button>
      <Button
        type="button"
        size="icon-sm"
        variant="ghost"
        aria-label="Zoom out"
        title="Zoom out"
        disabled={!range}
        onClick={() => onChange(zoomRange(domain, range, 2))}
      >
        <ZoomOut size={14} />
      </Button>
      <Button
        type="button"
        size="icon-sm"
        variant="ghost"
        aria-label="Reset range"
        title="Reset range"
        disabled={
          range?.lower === resetRange?.lower &&
          range?.upper === resetRange?.upper
        }
        onClick={onReset}
      >
        <RotateCcw size={14} />
      </Button>
    </form>
  );
}

function ImagePreview({
  image,
  loadImage,
}: {
  image: ChartImage;
  loadImage: (image: ChartImage) => {
    promise: Promise<ResultsResponse>;
    cancel: () => void;
  };
}) {
  const [view, setView] = useState<EvidenceView | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    let active = true;
    setView(null);
    setError("");
    const request = loadImage(image);
    void request.promise
      .then((response) => {
        if (active && response.kind === "evidence") setView(response.data);
      })
      .catch((error) => {
        if (active) setError(String(error));
      });
    return () => {
      active = false;
      request.cancel();
    };
  }, [image, loadImage]);
  return (
    <figure className="min-w-0 rounded border border-border p-3">
      <figcaption
        title={image.experiment_id}
        className="mb-2 break-all text-xs text-muted-foreground"
      >
        {image.label} · {image.path}
      </figcaption>
      {view?.data_url && (
        <img
          src={view.data_url}
          alt={`${image.label}: ${image.path}`}
          className="max-h-96 max-w-full object-contain"
        />
      )}
      {view?.reason && (
        <p className="text-xs text-muted-foreground">{view.reason}</p>
      )}
      {error && (
        <p role="alert" className="text-xs text-destructive">
          {error}
        </p>
      )}
      {!view && !error && (
        <p role="status" className="text-xs text-muted-foreground">
          Loading retained image…
        </p>
      )}
    </figure>
  );
}
