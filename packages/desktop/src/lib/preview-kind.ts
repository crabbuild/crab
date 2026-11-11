// Decide which previewer to use for a given file.
//
// Chosen by extension (fast path) plus a handful of special basenames
// (README, Dockerfile, etc.). Intentionally conservative: when in
// doubt we fall through to the Monaco text editor, which handles
// unknown files gracefully.

import { EntryKind } from "../ipc";

export type PreviewKind =
  | "image"
  | "video"
  | "audio"
  | "markdown"
  | "card-editor"
  | "json"
  | "jsonl"
  | "csv"
  | "tsv"
  | "parquet"
  | "arrow"
  | "safetensors"
  | "npy"
  | "npz"
  | "zip"
  | "pdf"
  | "notebook"
  | "html"
  | "geojson"
  | "sqlite"
  | "onnx"
  | "hdf5"
  | "protobuf"
  | "model3d"
  | "torch-checkpoint"
  | "text"
  | "pointer"
  | "binary";

const EXT_KIND: Record<string, PreviewKind> = {
  // Images Chromium can render natively under `file://`.
  png: "image",
  jpg: "image",
  jpeg: "image",
  gif: "image",
  webp: "image",
  avif: "image",
  bmp: "image",
  ico: "image",
  svg: "image",

  // Video — HTML5 <video>.
  mp4: "video",
  webm: "video",
  mov: "video",
  mkv: "video",
  avi: "video",

  // Audio — HTML5 <audio>.
  mp3: "audio",
  wav: "audio",
  flac: "audio",
  ogg: "audio",
  aac: "audio",
  m4a: "audio",
  wma: "audio",

  // Documents / structured text.
  md: "markdown",
  markdown: "markdown",
  mdx: "markdown",
  json: "json",
  jsonl: "jsonl",
  ndjson: "jsonl",
  csv: "csv",
  tsv: "tsv",
  pdf: "pdf",
  html: "html",
  htm: "html",
  ipynb: "notebook",

  // Geo.
  geojson: "geojson",

  // ML formats with agent-side parsers.
  parquet: "parquet",
  pq: "parquet",
  arrow: "arrow",
  feather: "arrow",
  ipc: "arrow",
  safetensors: "safetensors",
  npy: "npy",
  npz: "npz",
  zip: "zip",
  onnx: "onnx",

  // PyTorch checkpoints.
  pt: "torch-checkpoint",
  pth: "torch-checkpoint",
  ckpt: "torch-checkpoint",

  // Data stores.
  db: "sqlite",
  sqlite: "sqlite",
  sqlite3: "sqlite",

  // HDF5.
  h5: "hdf5",
  hdf5: "hdf5",
  hdf: "hdf5",

  // Protobuf.
  pb: "protobuf",
  desc: "protobuf",
  proto: "text", // .proto source files are just text

  // 3D models.
  obj: "model3d",
  stl: "model3d",
  gltf: "model3d",
  glb: "model3d",
  fbx: "model3d",
};

const BASENAME_KIND: Record<string, PreviewKind> = {
  README: "markdown",
  "README.md": "markdown",
  "README.MD": "markdown",
  Changelog: "markdown",
  CHANGELOG: "markdown",
  "CHANGELOG.md": "markdown",
  "MODEL_CARD.md": "card-editor",
  "MODEL_CARD.MD": "card-editor",
  "model_card.md": "card-editor",
  "Model_Card.md": "card-editor",
  "DATASET_CARD.md": "card-editor",
  "DATASET_CARD.MD": "card-editor",
  "dataset_card.md": "card-editor",
  "Dataset_Card.md": "card-editor",
};

export function previewKindFor(path: string, entryKind: EntryKind): PreviewKind {
  // Format routing is independent of storage. Captured-range previewers can
  // inspect pointers without hydrating; unknown formats open file metadata.

  const slash = path.lastIndexOf("/");
  const name = slash === -1 ? path : path.slice(slash + 1);
  const basenameMatch = BASENAME_KIND[name];
  if (basenameMatch) return basenameMatch;

  const dot = name.lastIndexOf(".");
  if (dot !== -1) {
    const ext = name.slice(dot + 1).toLowerCase();
    const extMatch = EXT_KIND[ext];
    if (extMatch) return extMatch;
  }

  // No known extension match. If it's a pointer/lfs entry, show
  // the pointer metadata card. Otherwise fall through to text.
  if (entryKind === "pointer" || entryKind === "lfs") return "pointer";
  return "text";
}
