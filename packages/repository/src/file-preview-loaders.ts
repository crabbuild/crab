import {
  extension,
  parseNumpyHeader,
  type TableData,
} from "./file-preview-model";
import { unzipSelected } from "./file-preview-office";

export async function loadNumpy(
  bytes: Uint8Array,
  name: string,
): Promise<TableData> {
  if (extension(name) === "npy") {
    const array = parseNumpyHeader(bytes);
    return {
      columns: ["Array", "Data type", "Shape", "Memory order", "Data bytes"],
      rows: [
        [
          name.split("/").pop() ?? name,
          array.dtype,
          array.shape.join(" × ") || "scalar",
          array.order,
          bytes.byteLength - array.dataOffset,
        ],
      ],
      totalRows: 1,
    };
  }
  const files = await unzipSelected(bytes, (path) =>
    path.toLowerCase().endsWith(".npy"),
  );
  const rows = Object.entries(files).map(([path, value]) => {
    const array = parseNumpyHeader(value);
    return [
      path,
      array.dtype,
      array.shape.join(" × ") || "scalar",
      array.order,
      value.byteLength - array.dataOffset,
    ];
  });
  return {
    columns: ["Array", "Data type", "Shape", "Memory order", "Data bytes"],
    rows,
    totalRows: rows.length,
  };
}

export async function loadGguf(bytes: Uint8Array): Promise<{
  metadata: TableData;
  tensors: TableData;
}> {
  const { ggufMetadata } = await import("hyllama");
  const value = ggufMetadata(
    bytes.buffer.slice(
      bytes.byteOffset,
      bytes.byteOffset + bytes.byteLength,
    ) as ArrayBuffer,
  );
  const metadataRows = Object.entries(value.metadata);
  return {
    metadata: {
      columns: ["Property", "Value"],
      rows: metadataRows,
      totalRows: metadataRows.length,
    },
    tensors: {
      columns: ["Tensor", "Dimensions", "Shape", "Type", "Offset"],
      rows: value.tensorInfos.map((tensor) => [
        tensor.name,
        tensor.nDims,
        tensor.shape.join(" × "),
        tensor.type,
        tensor.offset,
      ]),
      totalRows: value.tensorInfos.length,
    },
  };
}
