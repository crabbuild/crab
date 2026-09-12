import {
  extension,
  parseNumpyHeader,
  tableFromValues,
  type TableData,
} from "./file-preview-model";
import { unzipSelected } from "./file-preview-office";

export type DatabasePreview = {
  tables: Array<{
    name: string;
    type: string;
    definition: string;
    data: TableData;
  }>;
};

export async function loadSqlite(bytes: Uint8Array): Promise<DatabasePreview> {
  const [{ default: initSqlJs }, { default: wasmUrl }] = await Promise.all([
    import("sql.js"),
    import("sql.js/dist/sql-wasm.wasm?url"),
  ]);
  const SQL = await initSqlJs({ locateFile: () => wasmUrl });
  const database = new SQL.Database(bytes);
  try {
    const catalog = database.exec(
      "SELECT name, type, COALESCE(sql, '') FROM sqlite_master " +
        "WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 50",
    )[0];
    const tables = (catalog?.values ?? []).map(
      ([nameValue, typeValue, definitionValue]) => {
        const name = String(nameValue);
        const escaped = name.replaceAll('"', '""');
        const sample = database.exec(`SELECT * FROM "${escaped}" LIMIT 100`)[0];
        return {
          name,
          type: String(typeValue),
          definition: String(definitionValue),
          data: {
            columns: sample?.columns ?? [],
            rows: sample?.values ?? [],
            totalRows: sample?.values.length ?? 0,
            note: "Showing at most 100 rows. Crab runs only read queries against an in-browser copy.",
          },
        };
      },
    );
    return { tables };
  } finally {
    database.close();
  }
}

export async function loadParquet(bytes: Uint8Array): Promise<TableData> {
  const [{ parquetMetadataAsync, parquetReadObjects }, { compressors }] =
    await Promise.all([import("hyparquet"), import("hyparquet-compressors")]);
  const file = bytes.buffer.slice(
    bytes.byteOffset,
    bytes.byteOffset + bytes.byteLength,
  ) as ArrayBuffer;
  const metadata = await parquetMetadataAsync(file);
  const totalRows = Number(metadata.num_rows);
  const rows = await parquetReadObjects({
    file,
    compressors,
    rowStart: 0,
    rowEnd: Math.min(totalRows, 500),
  });
  return {
    ...tableFromValues(rows),
    totalRows,
    note:
      totalRows > rows.length
        ? `Showing ${rows.length.toLocaleString()} of ${totalRows.toLocaleString()} rows.`
        : undefined,
  };
}

export async function loadArrow(bytes: Uint8Array): Promise<TableData> {
  const { tableFromIPC } = await import("apache-arrow");
  const table = tableFromIPC(bytes);
  const columns = table.schema.fields.map((field) => field.name);
  const count = Math.min(table.numRows, 500);
  const vectors = columns.map((_, index) => table.getChildAt(index));
  const rows = Array.from({ length: count }, (_, row) =>
    vectors.map((vector) => vector?.get(row)),
  );
  return {
    columns,
    rows,
    totalRows: table.numRows,
    note:
      table.numRows > count
        ? `Showing ${count.toLocaleString()} of ${table.numRows.toLocaleString()} rows.`
        : undefined,
  };
}

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
