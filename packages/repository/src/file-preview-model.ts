export type PreviewKind =
  | "markdown"
  | "image"
  | "pdf"
  | "audio"
  | "video"
  | "delimited"
  | "json"
  | "notebook"
  | "office"
  | "sqlite"
  | "parquet"
  | "arrow"
  | "numpy"
  | "safetensors"
  | "gguf"
  | "archive"
  | "generic";

export type PreviewDescriptor = {
  kind: PreviewKind;
  label: string;
  mime?: string;
};

export type TableData = {
  columns: string[];
  rows: unknown[][];
  totalRows: number;
  note?: string;
};

const types: Record<string, PreviewDescriptor> = {
  md: { kind: "markdown", label: "Markdown" },
  markdown: { kind: "markdown", label: "Markdown" },
  png: { kind: "image", label: "PNG image", mime: "image/png" },
  jpg: { kind: "image", label: "JPEG image", mime: "image/jpeg" },
  jpeg: { kind: "image", label: "JPEG image", mime: "image/jpeg" },
  gif: { kind: "image", label: "GIF image", mime: "image/gif" },
  webp: { kind: "image", label: "WebP image", mime: "image/webp" },
  avif: { kind: "image", label: "AVIF image", mime: "image/avif" },
  bmp: { kind: "image", label: "Bitmap image", mime: "image/bmp" },
  ico: { kind: "image", label: "Icon", mime: "image/x-icon" },
  svg: { kind: "image", label: "SVG image", mime: "image/svg+xml" },
  pdf: { kind: "pdf", label: "PDF document", mime: "application/pdf" },
  mp3: { kind: "audio", label: "MP3 audio", mime: "audio/mpeg" },
  wav: { kind: "audio", label: "Wave audio", mime: "audio/wav" },
  flac: { kind: "audio", label: "FLAC audio", mime: "audio/flac" },
  ogg: { kind: "audio", label: "Ogg media", mime: "audio/ogg" },
  m4a: { kind: "audio", label: "MPEG-4 audio", mime: "audio/mp4" },
  aac: { kind: "audio", label: "AAC audio", mime: "audio/aac" },
  mp4: { kind: "video", label: "MPEG-4 video", mime: "video/mp4" },
  m4v: { kind: "video", label: "MPEG-4 video", mime: "video/mp4" },
  webm: { kind: "video", label: "WebM video", mime: "video/webm" },
  mov: { kind: "video", label: "QuickTime video", mime: "video/quicktime" },
  csv: { kind: "delimited", label: "CSV dataset" },
  tsv: { kind: "delimited", label: "TSV dataset" },
  json: { kind: "json", label: "JSON data" },
  jsonl: { kind: "json", label: "JSON Lines data" },
  ndjson: { kind: "json", label: "JSON Lines data" },
  ipynb: { kind: "notebook", label: "Jupyter notebook" },
  docx: { kind: "office", label: "Word document" },
  xlsx: { kind: "office", label: "Excel workbook" },
  pptx: { kind: "office", label: "PowerPoint presentation" },
  odt: { kind: "office", label: "OpenDocument text" },
  ods: { kind: "office", label: "OpenDocument spreadsheet" },
  odp: { kind: "office", label: "OpenDocument presentation" },
  sqlite: { kind: "sqlite", label: "SQLite database" },
  sqlite3: { kind: "sqlite", label: "SQLite database" },
  db: { kind: "sqlite", label: "SQLite database" },
  parquet: { kind: "parquet", label: "Parquet dataset" },
  arrow: { kind: "arrow", label: "Arrow dataset" },
  feather: { kind: "arrow", label: "Feather dataset" },
  ipc: { kind: "arrow", label: "Arrow IPC dataset" },
  npy: { kind: "numpy", label: "NumPy array" },
  npz: { kind: "numpy", label: "NumPy archive" },
  safetensors: { kind: "safetensors", label: "Safetensors model" },
  gguf: { kind: "gguf", label: "GGUF model" },
  zip: { kind: "archive", label: "ZIP archive" },
  jar: { kind: "archive", label: "Java archive" },
  war: { kind: "archive", label: "Web archive" },
  apk: { kind: "archive", label: "Android package" },
  ipa: { kind: "archive", label: "iOS package" },
  whl: { kind: "archive", label: "Python wheel" },
  pt: { kind: "archive", label: "PyTorch archive" },
  pth: { kind: "archive", label: "PyTorch artifact" },
  tar: { kind: "archive", label: "TAR archive" },
};

export function extension(name: string) {
  return name.split(/[?#]/, 1)[0].split(".").pop()?.toLowerCase() ?? "";
}

export function previewDescriptor(name: string) {
  return types[extension(name)] ?? null;
}

const MAX_TABLE_ROWS = 5_000;

export function tableFromValues(values: unknown[]): TableData {
  if (!values.length) return { columns: [], rows: [], totalRows: 0 };
  const objects = values.every(
    (value) =>
      value !== null && typeof value === "object" && !Array.isArray(value),
  );
  if (!objects)
    return {
      columns: ["Value"],
      rows: values.slice(0, MAX_TABLE_ROWS).map((value) => [value]),
      totalRows: values.length,
    };
  const columns = Array.from(
    new Set(
      values
        .slice(0, MAX_TABLE_ROWS)
        .flatMap((value) => Object.keys(value as Record<string, unknown>)),
    ),
  );
  return {
    columns,
    rows: values
      .slice(0, MAX_TABLE_ROWS)
      .map((value) =>
        columns.map((column) => (value as Record<string, unknown>)[column]),
      ),
    totalRows: values.length,
    note:
      values.length > MAX_TABLE_ROWS
        ? `Showing the first ${MAX_TABLE_ROWS.toLocaleString()} rows.`
        : undefined,
  };
}

export function parseSafetensors(bytes: Uint8Array): TableData {
  if (bytes.byteLength < 8) throw new Error("Safetensors header is incomplete");
  const headerLength = Number(
    new DataView(bytes.buffer, bytes.byteOffset, 8).getBigUint64(0, true),
  );
  if (
    !Number.isSafeInteger(headerLength) ||
    headerLength > bytes.byteLength - 8
  )
    throw new Error("Safetensors header length is invalid");
  const header = JSON.parse(
    new TextDecoder().decode(bytes.subarray(8, 8 + headerLength)),
  ) as Record<
    string,
    { dtype?: string; shape?: number[]; data_offsets?: [number, number] }
  >;
  const tensors = Object.entries(header).filter(
    ([name]) => name !== "__metadata__",
  );
  return {
    columns: ["Tensor", "Data type", "Shape", "Bytes"],
    rows: tensors.map(([name, tensor]) => [
      name,
      tensor.dtype ?? "—",
      tensor.shape?.join(" × ") ?? "—",
      tensor.data_offsets
        ? tensor.data_offsets[1] - tensor.data_offsets[0]
        : "—",
    ]),
    totalRows: tensors.length,
  };
}

export type NumpyArray = {
  dtype: string;
  shape: number[];
  order: "C" | "Fortran";
  dataOffset: number;
};

export function parseNumpyHeader(bytes: Uint8Array): NumpyArray {
  if (
    bytes.byteLength < 10 ||
    bytes[0] !== 0x93 ||
    new TextDecoder().decode(bytes.subarray(1, 6)) !== "NUMPY"
  )
    throw new Error("NumPy file signature is invalid");
  const major = bytes[6];
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const headerSize =
    major === 1 ? view.getUint16(8, true) : view.getUint32(8, true);
  const dataOffset = major === 1 ? 10 + headerSize : 12 + headerSize;
  if (dataOffset > bytes.byteLength)
    throw new Error("NumPy header is incomplete");
  const header = new TextDecoder("latin1").decode(
    bytes.subarray(major === 1 ? 10 : 12, dataOffset),
  );
  const dtype = /['"]descr['"]\s*:\s*['"]([^'"]+)['"]/.exec(header)?.[1];
  const shapeText = /['"]shape['"]\s*:\s*\(([^)]*)\)/.exec(header)?.[1];
  if (!dtype || shapeText === undefined)
    throw new Error("NumPy header is unsupported");
  return {
    dtype,
    shape: shapeText
      .split(",")
      .map((value) => Number(value.trim()))
      .filter(Number.isFinite),
    order: /['"]fortran_order['"]\s*:\s*True/.test(header) ? "Fortran" : "C",
    dataOffset,
  };
}

export function displayCell(value: unknown) {
  if (value === null || value === undefined) return "—";
  if (typeof value === "bigint") return value.toString();
  if (value instanceof Uint8Array) return `<${value.byteLength} bytes>`;
  if (value instanceof Date) return value.toISOString();
  if (typeof value === "object") {
    try {
      return JSON.stringify(value, (_, nested) =>
        typeof nested === "bigint" ? nested.toString() : nested,
      );
    } catch {
      return String(value);
    }
  }
  return String(value);
}
