const data = JSON.parse(document.getElementById("results-data").textContent);
document.getElementById("identity").textContent =
  "Immutable report " + data.id + " · " + data.report.created_at;
const table = document.createElement("table");
const head = table.createTHead().insertRow();
const rows = data.report.comparison.rows;
function runLabel(run) {
  return (run.name ? run.name + " · " : "") + run.experiment_id;
}
const keys = rows[0]?.cells.map((c) => c.key) || [];
for (const label of ["Run", "Status", ...keys]) {
  const th = document.createElement("th");
  th.textContent = label;
  head.append(th);
}
for (const row of rows) {
  const tr = table.insertRow();
  const run = row.run.snapshot;
  for (const value of [
    runLabel(run),
    run.status,
    ...row.cells.map((c) => {
      const v = c.candidate;
      return (
        (v?.state === "present"
          ? JSON.stringify(v.value)
          : v?.state || "Missing") +
        (c.absolute_delta === null ? "" : " (Δ " + c.absolute_delta + ")") +
        (c.reason ? " · " + c.reason : "")
      );
    }),
  ]) {
    const td = tr.insertCell();
    td.textContent = value;
  }
}
document.getElementById("comparison").append(table);
const views = [];
const controls = [];
const ranges = new Map(Object.entries(data.report.comparison.spec.axes));
const cursors = new Map();
const cursor = document.createElement("p");
cursor.setAttribute("role", "status");
document.getElementById("charts").append(cursor);
const showError = (error) => {
  cursor.textContent = String(error);
};
const savedRange = (key) => data.report.comparison.spec.axes[key] ?? null;
const sameRange = (left, right) =>
  left?.lower === right?.lower && left?.upper === right?.upper;
function changeRange(axis, range) {
  ranges.set(axis.key, range);
  cursors.delete(axis.key);
  cursor.textContent = "";
  for (const control of controls)
    if (control.key === axis.key) control.update();
  for (const peer of views)
    if (peer.axis?.key === axis.key)
      peer.view
        .signal("crab_shared_x", null)
        .signal("crab_domain", range ? [range.lower, range.upper] : null)
        .runAsync()
        .catch(showError);
}
function axisControls(section, chart) {
  const axis = chart.interaction;
  if (!axis?.domain) return;
  const form = document.createElement("form");
  form.setAttribute("aria-label", "X range " + chart.title);
  const caption = document.createElement("p");
  caption.textContent =
    "Linked x axis · " +
    axis.label +
    " · data " +
    axis.domain.lower +
    "–" +
    axis.domain.upper;
  form.append(caption);
  const input = (name, text) => {
    const label = document.createElement("label");
    label.textContent = text + " ";
    const field = document.createElement("input");
    field.type = "number";
    field.step = "any";
    field.placeholder = "Auto";
    field.setAttribute("aria-label", name + " " + chart.title);
    field.style.width = "7em";
    field.style.marginRight = "8px";
    label.append(field);
    form.append(label);
    return field;
  };
  const lower = input("X minimum", "From");
  const upper = input("X maximum", "To");
  const button = (text, action) => {
    const button = document.createElement("button");
    button.textContent = text;
    button.type = "button";
    button.onclick = action;
    form.append(button);
    return button;
  };
  const draft = () => ({
    lower: lower.value.trim() ? Number(lower.value) : axis.domain.lower,
    upper: upper.value.trim() ? Number(upper.value) : axis.domain.upper,
  });
  const apply = button("Apply range", () => {
    if (crabResults.validRange(axis.domain, draft()))
      changeRange(axis, draft());
  });
  button("Zoom in", () =>
    changeRange(
      axis,
      crabResults.zoomRange(axis.domain, ranges.get(axis.key) ?? null, 0.5),
    ),
  );
  const out = button("Zoom out", () =>
    changeRange(
      axis,
      crabResults.zoomRange(axis.domain, ranges.get(axis.key) ?? null, 2),
    ),
  );
  const reset = button("Reset range", () =>
    changeRange(axis, savedRange(axis.key)),
  );
  const validate = () => {
    apply.disabled = !crabResults.validRange(axis.domain, draft());
  };
  lower.oninput = upper.oninput = validate;
  form.onsubmit = (event) => {
    event.preventDefault();
    apply.click();
  };
  const update = () => {
    const range = ranges.get(axis.key) ?? null;
    lower.value = range ? String(range.lower) : "";
    upper.value = range ? String(range.upper) : "";
    out.disabled = !range;
    reset.disabled = sameRange(range, savedRange(axis.key));
    validate();
  };
  controls.push({ key: axis.key, update });
  update();
  section.append(form);
}
for (const chart of data.charts) {
  const section = document.createElement("section");
  const h = document.createElement("h2");
  h.textContent = chart.title;
  section.append(h);
  document.getElementById("charts").append(section);
  axisControls(section, chart);
  if (chart.reason) {
    const p = document.createElement("p");
    p.textContent = chart.reason;
    section.append(p);
  }
  if (chart.reduction === "counted") {
    const notice = document.createElement("p");
    notice.className = "muted";
    notice.textContent = `Counted all ${chart.points} retained observations into ${chart.displayed_points} observed class pairs. Missing class combinations are shown as zero.`;
    section.append(notice);
  }
  if (chart.reduction === "sampled") {
    const notice = document.createElement("p");
    notice.className = "muted";
    notice.textContent = `Showing ${chart.displayed_points} sampled points of ${chart.points}. Source bytes remain retained.`;
    section.append(notice);
  }
  for (const image of chart.images) {
    const body = data.objects[image.hash];
    const figure = document.createElement("figure");
    const caption = document.createElement("figcaption");
    caption.textContent = image.label + " · " + image.path;
    figure.append(caption);
    if (body) {
      const img = document.createElement("img");
      const ext = image.path.split(".").pop().toLowerCase();
      img.src =
        "data:image/" +
        (ext === "svg" ? "svg+xml" : ext === "jpg" ? "jpeg" : ext) +
        ";base64," +
        body;
      img.alt = caption.textContent;
      img.style.maxWidth = "100%";
      figure.append(img);
    }
    section.append(figure);
  }
  if (!chart.spec) continue;
  // Vega owns its host's ARIA attributes. The product owns the surrounding
  // keyboard-scrollable viewport and keeps export controls outside its scroll.
  const viewport = document.createElement("div");
  viewport.style.overflow = "auto";
  viewport.tabIndex = 0;
  viewport.setAttribute("role", "region");
  viewport.setAttribute("aria-label", chart.title);
  const el = document.createElement("div");
  viewport.append(el);
  section.append(viewport);
  const axis = chart.interaction;
  const showPlotError = (error) => {
    const index = views.findIndex((entry) => entry.host === el);
    if (index !== -1) views.splice(index, 1);
    const message = document.createElement("p");
    message.setAttribute("role", "alert");
    message.style.overflowWrap = "anywhere";
    message.textContent = String(error);
    el.replaceChildren(message);
  };
  vegaEmbed(el, chart.spec, {
    actions: true,
    interaction: axis,
    onError: showPlotError,
  })
    .then(async (result) => {
      if (result.actions) section.append(result.actions);
      views.push({ view: result.view, axis, host: el });
      if (!axis) return;
      result.view.addSignalListener("crab_cursor", (_name, value) => {
        const x = value ?? null;
        cursors.set(axis.key, x);
        cursor.textContent =
          x === null ? "" : "Cursor " + axis.field + ": " + x;
        for (const peer of views)
          if (peer.axis?.key === axis.key)
            peer.view.signal("crab_shared_x", x).runAsync().catch(showError);
      });
      result.view.signal("crab_shared_x", cursors.get(axis.key) ?? null);
      const range = ranges.get(axis.key);
      if (axis.domain)
        result.view.signal(
          "crab_domain",
          range ? [range.lower, range.upper] : null,
        );
      await result.view.runAsync();
    })
    .catch(showPlotError);
}
for (const row of rows) {
  for (const attachment of row.run.snapshot.attachments) {
    if (attachment.kind === "log" && !data.report.comparison.spec.include_logs)
      continue;
    const line = document.createElement("p");
    line.textContent =
      runLabel(row.run.snapshot) +
      " · " +
      attachment.path +
      " · " +
      attachment.state +
      " ";
    const body = data.objects[attachment.hash];
    if (!body && ["input", "output", "checkpoint"].includes(attachment.kind))
      line.append(
        "Immutable link; payload bytes are not retained by this report. ",
      );
    if (body) {
      const link = document.createElement("a");
      link.textContent = "Download";
      link.href = "data:application/octet-stream;base64," + body;
      link.download = attachment.path.split("/").pop();
      line.append(link);
      const preview = document.createElement("button");
      preview.textContent = "Preview";
      preview.style.marginLeft = "12px";
      preview.onclick = () => {
        const host = document.getElementById("preview");
        host.replaceChildren();
        const mime = attachment.path.match(/\.(png|jpe?g|gif|webp)$/i);
        if (mime) {
          const img = document.createElement("img");
          const ext = mime[1].toLowerCase();
          img.src =
            "data:image/" + (ext === "jpg" ? "jpeg" : ext) + ";base64," + body;
          img.alt = attachment.path;
          img.style.maxWidth = "100%";
          host.append(img);
        } else {
          const pre = document.createElement("pre");
          try {
            pre.textContent =
              attachment.size > 1048576
                ? "Text preview exceeds 1 MiB. Download the retained file."
                : new TextDecoder("utf-8", { fatal: true }).decode(
                    Uint8Array.from(atob(body), (c) => c.charCodeAt(0)),
                  );
          } catch {
            pre.textContent = "Binary evidence. Download the retained file.";
          }
          host.append(pre);
        }
      };
      line.append(preview);
    }
    document.getElementById("attachments").append(line);
  }
}
document.getElementById("evidence").textContent = JSON.stringify(
  data.report,
  null,
  2,
);
