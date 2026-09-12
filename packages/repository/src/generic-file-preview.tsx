import { extension } from "./file-preview-model";

const SAMPLE_BYTES = 64 * 1024;
const HEX_BYTES = 512;

const formatLabels: Record<string, string> = {
  avro: "Apache Avro container",
  bin: "Binary data",
  dicom: "DICOM medical image",
  dcm: "DICOM medical image",
  dll: "Windows library",
  dylib: "macOS library",
  exe: "Windows executable",
  h5: "HDF5 dataset",
  hdf5: "HDF5 dataset",
  joblib: "Joblib artifact",
  onnx: "ONNX model",
  orc: "Apache ORC dataset",
  pickle: "Python pickle",
  pkl: "Python pickle",
  protobuf: "Protocol Buffers data",
  proto3: "Protocol Buffers data",
  so: "Shared library",
};

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes.toLocaleString()} bytes`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function startsWith(bytes: Uint8Array, signature: number[]) {
  return signature.every((byte, index) => bytes[index] === byte);
}

function signature(bytes: Uint8Array) {
  if (startsWith(bytes, [0x7f, 0x45, 0x4c, 0x46])) return "ELF executable";
  if (startsWith(bytes, [0x4d, 0x5a])) return "Portable Executable";
  if (startsWith(bytes, [0x1f, 0x8b])) return "Gzip stream";
  if (startsWith(bytes, [0x42, 0x5a, 0x68])) return "Bzip2 stream";
  if (startsWith(bytes, [0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c]))
    return "7-Zip archive";
  if (startsWith(bytes, [0x89, 0x48, 0x44, 0x46, 0x0d, 0x0a, 0x1a, 0x0a]))
    return "HDF5 container";
  if (startsWith(bytes, [0x4f, 0x62, 0x6a, 0x01]))
    return "Apache Avro container";
  if (startsWith(bytes, [0x4f, 0x52, 0x43])) return "Apache ORC dataset";
  if (
    bytes.length >= 132 &&
    String.fromCharCode(...bytes.subarray(128, 132)) === "DICM"
  )
    return "DICOM file";
  return "No known magic signature";
}

function textSample(bytes: Uint8Array) {
  const sample = bytes.subarray(0, Math.min(bytes.length, SAMPLE_BYTES));
  if (sample.includes(0)) return null;
  try {
    const text = new TextDecoder("utf-8", { fatal: true }).decode(sample);
    const controls = Array.from(text).filter(
      (character) => character < " " && !"\n\r\t".includes(character),
    ).length;
    return controls / Math.max(text.length, 1) < 0.02 ? text : null;
  } catch {
    return null;
  }
}

function hexDump(bytes: Uint8Array) {
  const sample = bytes.subarray(0, Math.min(bytes.length, HEX_BYTES));
  const lines: string[] = [];
  for (let offset = 0; offset < sample.length; offset += 16) {
    const row = sample.subarray(offset, offset + 16);
    const hex = Array.from(row, (byte) => byte.toString(16).padStart(2, "0"))
      .join(" ")
      .padEnd(47);
    const ascii = Array.from(row, (byte) =>
      byte >= 32 && byte <= 126 ? String.fromCharCode(byte) : ".",
    ).join("");
    lines.push(`${offset.toString(16).padStart(8, "0")}  ${hex}  |${ascii}|`);
  }
  return lines.join("\n");
}

export function GenericFilePreview({
  bytes,
  name,
}: {
  bytes: Uint8Array;
  name: string;
}) {
  const suffix = extension(name);
  const text = textSample(bytes);
  return (
    <div className="generic-file-preview">
      <dl className="file-facts">
        <div>
          <dt>Format</dt>
          <dd>
            {formatLabels[suffix] ??
              (suffix ? `${suffix.toUpperCase()} file` : "Binary file")}
          </dd>
        </div>
        <div>
          <dt>Size</dt>
          <dd>{formatBytes(bytes.length)}</dd>
        </div>
        <div>
          <dt>Signature</dt>
          <dd>{signature(bytes)}</dd>
        </div>
        <div>
          <dt>Inspection</dt>
          <dd>
            First {Math.min(bytes.length, SAMPLE_BYTES).toLocaleString()} bytes
          </dd>
        </div>
      </dl>
      {text !== null && (
        <section className="generic-preview-section">
          <h3>Text sample</h3>
          <pre>{text}</pre>
        </section>
      )}
      <section className="generic-preview-section">
        <h3>Hex and ASCII</h3>
        <pre>{hexDump(bytes)}</pre>
        {bytes.length > HEX_BYTES && (
          <p>Hex view is limited to the first {HEX_BYTES} bytes.</p>
        )}
      </section>
    </div>
  );
}
