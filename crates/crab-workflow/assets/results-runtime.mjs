import { parse, View, loader, logger, Error as ErrorLevel } from "vega";
import { compile } from "vega-lite";
import { expressionInterpreter } from "vega-interpreter";

// Retained charts use inline values. Neither data loading nor link navigation
// may reach a server, even when a custom template was authored with a URL.
const offlineLoader = {
  ...loader(),
  load: async () => {
    throw new Error("External chart data is unavailable offline");
  },
  sanitize: async () => {
    throw new Error("External chart links are disabled");
  },
};

export function validRange(domain, range) {
  return (
    Number.isFinite(range.lower) &&
    Number.isFinite(range.upper) &&
    range.lower < range.upper &&
    Number.isFinite(range.upper - range.lower) &&
    range.lower >= domain.lower &&
    range.upper <= domain.upper
  );
}

export function zoomRange(domain, range, factor) {
  const current = range ?? domain;
  const center = current.lower + (current.upper - current.lower) / 2;
  const half = ((current.upper - current.lower) * factor) / 2;
  const next = {
    lower: Math.max(domain.lower, center - half),
    upper: Math.min(domain.upper, center + half),
  };
  if (next.lower === domain.lower && next.upper === domain.upper) return null;
  return validRange(domain, next) ? next : range;
}
export async function render(target, spec, options) {
  const element =
    typeof target === "string" ? document.querySelector(target) : target;
  if (!element) throw new Error("Chart container is missing");
  element.classList.add("crab-results-chart");
  element.toggleAttribute(
    "data-crab-dashed-series",
    Boolean(
      spec.encoding?.strokeDash?.field &&
      spec.encoding.strokeDash.field === spec.encoding?.color?.field,
    ),
  );
  // Forced colors also replace data-series paint; ordinary themes inherit
  // guide paint without rebuilding the view or changing authored series colors.
  if (!document.getElementById("crab-results-chart-colors")) {
    const colors = document.createElement("style");
    colors.id = "crab-results-chart-colors";
    colors.textContent = `@media (forced-colors: active) {
      .crab-results-chart svg text { fill: CanvasText !important; }
      .crab-results-chart svg .role-axis line,
      .crab-results-chart svg .role-axis path,
      .crab-results-chart svg .mark-rule.role-mark line { stroke: CanvasText !important; }
      .crab-results-chart[data-crab-dashed-series] svg .mark-line path,
      .crab-results-chart[data-crab-dashed-series] svg .role-legend-symbol path { stroke: CanvasText !important; }
    }`;
    document.head.append(colors);
  }
  const style = getComputedStyle(element);
  const lite = String(spec.$schema || "").includes("vega-lite");
  // Vega-Lite 5 supports fit-x on unit/layer views, not facets or concatenation.
  // Keep those authored layouts scrollable instead of changing their semantics.
  const fluid =
    lite &&
    (spec.mark || spec.layer) &&
    !spec.facet &&
    !spec.repeat &&
    !spec.concat &&
    !spec.hconcat &&
    !spec.vconcat &&
    !spec.encoding?.row &&
    !spec.encoding?.column;
  const themed = {
    ...spec,
    ...(fluid
      ? {
          width: element.clientWidth,
          autosize: { type: "fit-x", contains: "padding", resize: true },
        }
      : {}),
    config: {
      background: "transparent",
      font: style.fontFamily,
      ...spec.config,
      axis: {
        labelColor: "currentColor",
        titleColor: "currentColor",
        domainColor: "currentColor",
        tickColor: "currentColor",
        gridColor: "currentColor",
        gridOpacity: 0.15,
        ...spec.config?.axis,
      },
      legend: {
        labelColor: "currentColor",
        titleColor: "currentColor",
        orient: "bottom",
        direction: "vertical",
        columns: 1,
        labelLimit: Math.max(80, element.clientWidth - 50),
        ...spec.config?.legend,
      },
      title: { color: "currentColor", ...spec.config?.title },
    },
  };
  const compiled = lite ? compile(themed).spec : themed;
  if (options.interaction) {
    // Vega-Lite rewrites resolved selection keys for literal field names.
    // The owned single-field tuple provides the cursor without those keys.
    (compiled.signals ??= []).push({
      name: "crab_cursor",
      update: "crab_hover_tuple ? crab_hover_tuple.values[0] : null",
    });
  }
  if (options.interaction?.domain) {
    const xScales = new Set();
    const references = (value) => {
      if (Array.isArray(value)) value.forEach(references);
      else if (
        value &&
        typeof value === "object" &&
        typeof value.scale === "string"
      )
        xScales.add(value.scale);
    };
    const marks = (items) =>
      items?.forEach((mark) => {
        for (const encoding of Object.values(mark.encode ?? {}))
          references(encoding.x);
        marks(mark.marks);
      });
    marks(compiled.marks);
    const scales = (scope) => {
      for (const scale of scope.scales ?? []) {
        if (!xScales.has(scale.name)) continue;
        // Vega's domainRaw bypasses nice/zero only while zoom is active.
        // Preserve authored scale bindings and reset to them when cleared.
        const original = scale.domainRaw;
        scale.domainRaw = {
          signal: original
            ? `isValid(crab_domain) ? crab_domain : (${original.signal ?? JSON.stringify(original)})`
            : "crab_domain",
        };
        if (
          scale.domain &&
          !Array.isArray(scale.domain) &&
          (scale.domain.data || scale.domain.fields)
        )
          scale.domain = { signal: "crab_extent" };
      }
      for (const mark of scope.marks ?? []) scales(mark);
    };
    scales(compiled);
  }
  // Vega-Lite 5's nearest-selection compiler emits transparent Voronoi hit
  // regions without inheriting mark aria:false. They carry no result content.
  const accessibleMarks = (marks) =>
    marks?.map((mark) => ({
      ...mark,
      ...(mark.transform?.some((transform) => transform.type === "voronoi")
        ? { aria: false }
        : {}),
      ...(mark.marks ? { marks: accessibleMarks(mark.marks) } : {}),
    }));
  const accessible = { ...compiled, marks: accessibleMarks(compiled.marks) };
  const runtime = parse(accessible, null, { ast: true });
  let view;
  let observer;
  let actions;
  let disposed = false;
  let ready = false;
  let failure;
  const finalize = () => {
    if (disposed) return;
    disposed = true;
    observer?.disconnect();
    actions?.remove();
    view?.finalize();
  };
  const fail = (error) => {
    if (disposed || failure) return;
    failure = new Error(
      `Could not render this plot. Inspect its template and source data. ${String(error)}`,
      { cause: error },
    );
    if (ready) {
      finalize();
      element.replaceChildren();
      options.onError(failure);
    }
  };
  // Vega resolves runAsync after logging evaluation errors. Observe its logger
  // without throwing: event dispatch does not await that promise.
  const diagnostics = logger(ErrorLevel, undefined, (_method, _level, errors) =>
    fail(
      errors.length === 1 ? errors[0] : Array.from(errors, String).join(" "),
    ),
  );
  view = new View(runtime, {
    expr: expressionInterpreter,
    renderer: "svg",
    container: element,
    hover: true,
    loader: offlineLoader,
    logger: diagnostics,
  });
  try {
    await view.runAsync();
    if (failure) throw failure;
  } catch (error) {
    finalize();
    element.replaceChildren();
    throw error;
  }
  ready = true;
  let width = element.clientWidth;
  observer = fluid
    ? new ResizeObserver(() => {
        if (
          !disposed &&
          element.clientWidth > 0 &&
          element.clientWidth !== width
        ) {
          width = element.clientWidth;
          void view.width(width).resize().runAsync().catch(fail);
        }
      })
    : null;
  observer?.observe(element);
  if (options.actions) {
    actions = document.createElement("div");
    actions.setAttribute("aria-label", "Chart export");
    for (const type of ["svg", "png"]) {
      const button = document.createElement("button");
      button.textContent = `Save ${type.toUpperCase()}`;
      button.type = "button";
      button.style.marginRight = "8px";
      button.onclick = async () => {
        try {
          const url =
            type === "svg"
              ? URL.createObjectURL(
                  new Blob([await view.toSVG()], { type: "image/svg+xml" }),
                )
              : await view.toImageURL("png");
          const link = document.createElement("a");
          link.href = url;
          link.download = `chart.${type}`;
          link.click();
          if (type === "svg") setTimeout(() => URL.revokeObjectURL(url), 1000);
        } catch (error) {
          const message = document.createElement("p");
          message.textContent = String(error);
          actions.append(message);
        }
      };
      actions.append(button);
    }
    const source = document.createElement("details");
    const summary = document.createElement("summary");
    summary.textContent = "Chart source";
    const text = document.createElement("pre");
    text.textContent = JSON.stringify(spec, null, 2);
    source.append(summary, text);
    actions.append(source);
    element.append(actions);
  }
  return {
    view,
    spec,
    actions,
    finalize,
  };
}
window.vegaEmbed = render;
window.crabResults = { validRange, zoomRange };
