/**
 * Renderer-side metadata for each supported sidecar process.
 *
 * The actual spawn logic lives in `electron/sidecars.ts`; this module
 * provides the UI-facing labels, icons, and install hints so views can
 * render sidecar cards without importing electron internals.
 */

import type { SidecarId } from "../../electron/sidecars";

export interface SidecarDef {
  id: SidecarId;
  label: string;
  description: string;
  /** Lucide icon name. */
  icon: string;
  defaultPort: number;
  /** What to install if the command is missing. */
  installHint: string;
  docsUrl: string;
}

export const SIDECAR_DEFS: readonly SidecarDef[] = [
  {
    id: "jupyter",
    label: "Jupyter Lab",
    description: "Interactive notebook environment for data exploration and prototyping.",
    icon: "notebook-pen",
    defaultPort: 8888,
    installHint: "pip install jupyterlab",
    docsUrl: "https://jupyterlab.readthedocs.io/",
  },
  {
    id: "tensorboard",
    label: "TensorBoard",
    description: "Visualise training metrics, model graphs, and profiling data.",
    icon: "line-chart",
    defaultPort: 6006,
    installHint: "pip install tensorboard",
    docsUrl: "https://www.tensorflow.org/tensorboard",
  },
  {
    id: "mlflow-ui",
    label: "MLflow UI",
    description: "Track experiments, compare runs, and manage model registry.",
    icon: "flask-conical",
    defaultPort: 5000,
    installHint: "pip install mlflow",
    docsUrl: "https://mlflow.org/docs/latest/index.html",
  },
  {
    id: "duckdb",
    label: "DuckDB",
    description: "In-process SQL analytics engine with an HTTP API.",
    icon: "database",
    defaultPort: 4321,
    installHint: "brew install duckdb (macOS) or download from https://duckdb.org",
    docsUrl: "https://duckdb.org/docs/",
  },
] as const;

/** Look up a sidecar definition by ID. */
export function sidecarDef(id: SidecarId): SidecarDef | undefined {
  return SIDECAR_DEFS.find((d) => d.id === id);
}
