import { unzip } from "fflate";
import type { TableData } from "./file-preview-model";

const MAX_EXPANDED_BYTES = 30 * 1024 * 1024;
const decoder = new TextDecoder();

export type OfficePreview =
  | {
      kind: "document";
      sections: Array<{
        title?: string;
        paragraphs: string[];
        table?: TableData;
      }>;
    }
  | { kind: "workbook"; sheets: Array<{ name: string; table: TableData }> }
  | { kind: "presentation"; slides: Array<{ title: string; lines: string[] }> };

export function unzipSelected(
  bytes: Uint8Array,
  include: (name: string) => boolean,
) {
  return new Promise<Record<string, Uint8Array>>((resolve, reject) => {
    let expanded = 0;
    unzip(
      bytes,
      {
        filter(file) {
          if (!include(file.name)) return false;
          expanded += file.originalSize;
          return expanded <= MAX_EXPANDED_BYTES;
        },
      },
      (error, files) => {
        if (error) reject(error);
        else if (expanded > MAX_EXPANDED_BYTES)
          reject(new Error("Expanded preview data exceeds 30 MB"));
        else resolve(files);
      },
    );
  });
}

function document(bytes?: Uint8Array) {
  if (!bytes)
    throw new Error("The document package is missing required content");
  const value = new DOMParser().parseFromString(
    decoder.decode(bytes),
    "application/xml",
  );
  if (value.querySelector("parsererror"))
    throw new Error("Document XML is invalid");
  return value;
}

function descendants(node: ParentNode, localName: string) {
  return Array.from(node.querySelectorAll("*")).filter(
    (element) => element.localName === localName,
  );
}

function textRuns(node: ParentNode) {
  return descendants(node, "t")
    .map((element) => element.textContent ?? "")
    .join("")
    .trim();
}

function tableFromRows(rows: Element[]): TableData {
  const values = rows.map((row) =>
    descendants(row, "tc").map((cell) => textRuns(cell)),
  );
  const width = Math.max(0, ...values.map((row) => row.length));
  return {
    columns: Array.from({ length: width }, (_, index) => `Column ${index + 1}`),
    rows: values,
    totalRows: values.length,
  };
}

function spreadsheetColumnName(index: number) {
  let name = "";
  for (let value = index + 1; value > 0; value = Math.floor((value - 1) / 26))
    name = String.fromCharCode(65 + ((value - 1) % 26)) + name;
  return name;
}

async function wordPreview(bytes: Uint8Array): Promise<OfficePreview> {
  const files = await unzipSelected(
    bytes,
    (name) => name === "word/document.xml",
  );
  const body = descendants(document(files["word/document.xml"]), "body")[0];
  if (!body) throw new Error("Word document body is missing");
  const sections: Extract<OfficePreview, { kind: "document" }>["sections"] = [];
  for (const child of Array.from(body.children)) {
    if (child.localName === "p") {
      const text = textRuns(child);
      if (text) sections.push({ paragraphs: [text] });
    } else if (child.localName === "tbl") {
      sections.push({
        paragraphs: [],
        table: tableFromRows(descendants(child, "tr")),
      });
    }
  }
  return { kind: "document", sections };
}

function sharedStrings(files: Record<string, Uint8Array>) {
  const shared = files["xl/sharedStrings.xml"];
  if (!shared) return [];
  return descendants(document(shared), "si").map(textRuns);
}

function spreadsheetCell(cell: Element, shared: string[]) {
  const type = cell.getAttribute("t");
  const value =
    descendants(cell, type === "inlineStr" ? "t" : "v")[0]?.textContent ?? "";
  if (type === "s") return shared[Number(value)] ?? value;
  if (type === "b") return value === "1";
  if (type === "str" || type === "inlineStr") return value;
  const numeric = Number(value);
  return value !== "" && Number.isFinite(numeric) ? numeric : value;
}

function columnIndex(reference: string) {
  let value = 0;
  for (const character of reference.match(/^[A-Z]+/)?.[0] ?? "")
    value = value * 26 + character.charCodeAt(0) - 64;
  return Math.max(0, value - 1);
}

async function workbookPreview(bytes: Uint8Array): Promise<OfficePreview> {
  const files = await unzipSelected(
    bytes,
    (name) =>
      name === "xl/workbook.xml" ||
      name === "xl/sharedStrings.xml" ||
      /^xl\/worksheets\/sheet\d+\.xml$/.test(name),
  );
  const names = files["xl/workbook.xml"]
    ? descendants(document(files["xl/workbook.xml"]), "sheet").map(
        (sheet) => sheet.getAttribute("name") ?? "Sheet",
      )
    : [];
  const shared = sharedStrings(files);
  const paths = Object.keys(files)
    .filter((name) => /^xl\/worksheets\/sheet\d+\.xml$/.test(name))
    .sort((left, right) =>
      left.localeCompare(right, undefined, { numeric: true }),
    );
  const sheets = paths.map((path, sheetIndex) => {
    const rows = descendants(document(files[path]), "row").slice(0, 5_001);
    const values = rows.map((row) => {
      const cells = descendants(row, "c");
      const width = Math.max(
        0,
        ...cells.map((cell) => columnIndex(cell.getAttribute("r") ?? "A") + 1),
      );
      const result: unknown[] = Array.from({ length: width }, () => "");
      for (const cell of cells)
        result[columnIndex(cell.getAttribute("r") ?? "A")] = spreadsheetCell(
          cell,
          shared,
        );
      return result;
    });
    const width = Math.max(0, ...values.map((row) => row.length));
    return {
      name: names[sheetIndex] ?? `Sheet ${sheetIndex + 1}`,
      table: {
        columns: Array.from({ length: width }, (_, index) =>
          spreadsheetColumnName(index),
        ),
        rows: values,
        totalRows: values.length,
        note: rows.length > 5_000 ? "Showing the first 5,000 rows." : undefined,
      },
    };
  });
  return { kind: "workbook", sheets };
}

async function presentationPreview(bytes: Uint8Array): Promise<OfficePreview> {
  const files = await unzipSelected(bytes, (name) =>
    /^ppt\/slides\/slide\d+\.xml$/.test(name),
  );
  const paths = Object.keys(files).sort((left, right) =>
    left.localeCompare(right, undefined, { numeric: true }),
  );
  return {
    kind: "presentation",
    slides: paths.map((path, index) => {
      const lines = descendants(document(files[path]), "p")
        .map(textRuns)
        .filter(Boolean);
      return { title: lines[0] || `Slide ${index + 1}`, lines: lines.slice(1) };
    }),
  };
}

async function openDocumentPreview(
  bytes: Uint8Array,
  extension: string,
): Promise<OfficePreview> {
  const files = await unzipSelected(bytes, (name) => name === "content.xml");
  const content = document(files["content.xml"]);
  if (extension === "ods") {
    const sheets = descendants(content, "table").map((sheet, index) => {
      const rows = descendants(sheet, "table-row").slice(0, 5_000);
      return {
        name: sheet.getAttribute("table:name") ?? `Sheet ${index + 1}`,
        table: {
          columns: Array.from(
            {
              length: Math.max(
                0,
                ...rows.map((row) => descendants(row, "table-cell").length),
              ),
            },
            (_, column) => spreadsheetColumnName(column),
          ),
          rows: rows.map((row) =>
            descendants(row, "table-cell").map((cell) => textRuns(cell)),
          ),
          totalRows: rows.length,
        },
      };
    });
    return { kind: "workbook", sheets };
  }
  if (extension === "odp") {
    return {
      kind: "presentation",
      slides: descendants(content, "page").map((page, index) => {
        const lines = descendants(page, "p").map(textRuns).filter(Boolean);
        return {
          title: lines[0] || `Slide ${index + 1}`,
          lines: lines.slice(1),
        };
      }),
    };
  }
  return {
    kind: "document",
    sections: descendants(content, "p")
      .map(textRuns)
      .filter(Boolean)
      .map((text) => ({ paragraphs: [text] })),
  };
}

export async function parseOffice(
  bytes: Uint8Array,
  extension: string,
): Promise<OfficePreview> {
  if (extension === "docx") return wordPreview(bytes);
  if (extension === "xlsx") return workbookPreview(bytes);
  if (extension === "pptx") return presentationPreview(bytes);
  return openDocumentPreview(bytes, extension);
}

export async function archiveInventory(
  bytes: Uint8Array,
  extension: string,
): Promise<TableData> {
  if (extension === "tar") return tarInventory(bytes);
  const entries: Array<[string, number, number, string]> = [];
  await new Promise<void>((resolve, reject) => {
    unzip(
      bytes,
      {
        filter(file) {
          entries.push([
            file.name,
            file.originalSize,
            file.size,
            file.name.endsWith("/") ? "Directory" : "File",
          ]);
          return false;
        },
      },
      (error) => (error ? reject(error) : resolve()),
    );
  });
  return {
    columns: ["Path", "Bytes", "Compressed bytes", "Type"],
    rows: entries.slice(0, 5_000),
    totalRows: entries.length,
    note:
      entries.length > 5_000 ? "Showing the first 5,000 entries." : undefined,
  };
}

function tarInventory(bytes: Uint8Array): TableData {
  const rows: unknown[][] = [];
  for (let offset = 0; offset + 512 <= bytes.length && rows.length < 5_000; ) {
    const header = bytes.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    const name = decoder.decode(header.subarray(0, 100)).replace(/\0.*$/, "");
    const size = Number.parseInt(
      decoder.decode(header.subarray(124, 136)).replace(/\0.*$/, "").trim() ||
        "0",
      8,
    );
    const type = header[156] === 53 ? "Directory" : "File";
    rows.push([name, size, type]);
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  return {
    columns: ["Path", "Bytes", "Type"],
    rows,
    totalRows: rows.length,
    note:
      rows.length === 5_000 ? "Showing the first 5,000 entries." : undefined,
  };
}
