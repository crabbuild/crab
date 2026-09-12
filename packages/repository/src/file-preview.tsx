import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Spinner } from "@primer/react";
import type { Repository } from "./api";
import { DataTable } from "./data-explorer";
import { RepositoryMarkdown } from "./repository-markdown";
import {
  extension,
  parseDelimited,
  parseJsonTable,
  parseSafetensors,
  type PreviewDescriptor,
} from "./file-preview-model";
import {
  archiveInventory,
  parseOffice,
  type OfficePreview,
} from "./file-preview-office";
import {
  loadArrow,
  loadGguf,
  loadNumpy,
  loadParquet,
  loadSqlite,
  type DatabasePreview,
} from "./file-preview-loaders";
import { PdfPreview } from "./pdf-preview";
import { GenericFilePreview } from "./generic-file-preview";

const MAX_PREVIEW_BYTES = 50 * 1024 * 1024;

type Props = {
  descriptor: PreviewDescriptor;
  repo: Repository;
  rev: string;
  directory: string;
  name: string;
  text: string | null;
  size: number;
  blobUrl: string;
};

type BinaryState = {
  bytes?: Uint8Array;
  error?: string;
  loading: boolean;
};

function useBinary(url: string, size: number): BinaryState {
  const [state, setState] = useState<BinaryState>({ loading: true });
  useEffect(() => {
    const controller = new AbortController();
    if (size > MAX_PREVIEW_BYTES) {
      setState({
        loading: false,
        error: `Interactive previews are limited to ${formatBytes(MAX_PREVIEW_BYTES)}. Download this ${formatBytes(size)} file to inspect it locally.`,
      });
      return () => controller.abort();
    }
    setState({ loading: true });
    fetch(url, {
      signal: controller.signal,
      headers: { Accept: "application/octet-stream" },
    })
      .then(async (response) => {
        if (!response.ok)
          throw new Error(
            `File bytes could not be loaded (${response.status})`,
          );
        const buffer = await response.arrayBuffer();
        if (!controller.signal.aborted)
          setState({ loading: false, bytes: new Uint8Array(buffer) });
      })
      .catch((error: unknown) => {
        if (!controller.signal.aborted)
          setState({
            loading: false,
            error:
              error instanceof Error
                ? error.message
                : "File bytes could not be loaded",
          });
      });
    return () => controller.abort();
  }, [size, url]);
  return state;
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes.toLocaleString()} bytes`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function PreviewNotice({
  children,
  error = false,
}: {
  children: ReactNode;
  error?: boolean;
}) {
  return (
    <div
      className={`file-preview-notice${error ? " error" : ""}`}
      role={error ? "alert" : "status"}
    >
      {children}
    </div>
  );
}

function TextDataPreview({
  descriptor,
  name,
  text,
}: Pick<Props, "descriptor" | "name" | "text">) {
  try {
    const data =
      descriptor.kind === "delimited"
        ? parseDelimited(text ?? "", extension(name) === "tsv" ? "\t" : ",")
        : parseJsonTable(
            text ?? "",
            ["jsonl", "ndjson"].includes(extension(name)),
          );
    return <DataTable data={data} name={descriptor.label} />;
  } catch (error) {
    return (
      <PreviewNotice error>
        <strong>This file could not be parsed as {descriptor.label}.</strong>
        <p>{error instanceof Error ? error.message : "The data is invalid."}</p>
      </PreviewNotice>
    );
  }
}

type Notebook = {
  cells?: Array<{
    cell_type?: string;
    execution_count?: number | null;
    source?: string | string[];
    outputs?: Array<{
      output_type?: string;
      text?: string | string[];
      data?: Record<string, string | string[]>;
    }>;
  }>;
};

function sourceText(value?: string | string[]) {
  return Array.isArray(value) ? value.join("") : (value ?? "");
}

function NotebookPreview({
  repo,
  rev,
  directory,
  text,
}: Pick<Props, "repo" | "rev" | "directory" | "text">) {
  let notebook: Notebook;
  try {
    notebook = JSON.parse(text ?? "") as Notebook;
  } catch (error) {
    return (
      <PreviewNotice error>
        {error instanceof Error ? error.message : "Notebook JSON is invalid"}
      </PreviewNotice>
    );
  }
  return (
    <div className="notebook-preview">
      {(notebook.cells ?? []).slice(0, 200).map((cell, index) => (
        <section
          className={`notebook-cell ${cell.cell_type ?? "raw"}`}
          key={index}
        >
          <div className="notebook-gutter">
            {cell.cell_type === "code"
              ? `In [${cell.execution_count ?? " "}]`
              : "Text"}
          </div>
          <div className="notebook-content">
            {cell.cell_type === "markdown" ? (
              <RepositoryMarkdown repo={repo} rev={rev} directory={directory}>
                {sourceText(cell.source)}
              </RepositoryMarkdown>
            ) : (
              <pre>{sourceText(cell.source)}</pre>
            )}
            {cell.outputs?.map((output, outputIndex) => {
              const plain = sourceText(
                output.data?.["text/plain"] ?? output.text,
              );
              const image = sourceText(output.data?.["image/png"]);
              return (
                <div className="notebook-output" key={outputIndex}>
                  {image ? (
                    <img
                      src={`data:image/png;base64,${image.replaceAll(/\s/g, "")}`}
                      alt="Notebook output"
                    />
                  ) : (
                    <pre>{plain || `[${output.output_type ?? "output"}]`}</pre>
                  )}
                </div>
              );
            })}
          </div>
        </section>
      ))}
      {(notebook.cells?.length ?? 0) > 200 && (
        <PreviewNotice>Showing the first 200 notebook cells.</PreviewNotice>
      )}
    </div>
  );
}

function useObjectUrl(bytes: Uint8Array, mime: string) {
  const url = useMemo(
    () =>
      URL.createObjectURL(
        new Blob(
          [
            bytes.buffer.slice(
              bytes.byteOffset,
              bytes.byteOffset + bytes.byteLength,
            ) as ArrayBuffer,
          ],
          { type: mime },
        ),
      ),
    [bytes, mime],
  );
  useEffect(() => () => URL.revokeObjectURL(url), [url]);
  return url;
}

function MediaPreview({
  descriptor,
  bytes,
  name,
}: {
  descriptor: PreviewDescriptor;
  bytes: Uint8Array;
  name: string;
}) {
  const url = useObjectUrl(
    bytes,
    descriptor.mime ?? "application/octet-stream",
  );
  const [zoom, setZoom] = useState(100);
  if (descriptor.kind === "image")
    return (
      <div className="media-preview">
        <div className="media-preview-toolbar">
          <label>
            Zoom
            <input
              aria-label="Image zoom"
              type="range"
              min="25"
              max="300"
              step="25"
              value={zoom}
              onChange={(event) => setZoom(Number(event.target.value))}
            />
            <output>{zoom}%</output>
          </label>
        </div>
        <div className="image-stage" tabIndex={0}>
          <img
            src={url}
            alt={`Preview of ${name}`}
            style={{ width: `${zoom}%` }}
          />
        </div>
      </div>
    );
  if (descriptor.kind === "pdf")
    return <PdfPreview bytes={bytes} name={name} />;
  if (descriptor.kind === "audio")
    return (
      <div className="media-player">
        <audio src={url} controls aria-label={`Audio preview of ${name}`} />
      </div>
    );
  return (
    <div className="media-player video-player">
      <video src={url} controls aria-label={`Video preview of ${name}`} />
    </div>
  );
}

function AsyncValue<T>({
  cacheKey,
  load,
  children,
}: {
  cacheKey: string;
  load: () => Promise<T>;
  children: (value: T) => ReactNode;
}) {
  const loader = useRef(load);
  loader.current = load;
  const [state, setState] = useState<{ value?: T; error?: string }>({});
  useEffect(() => {
    let active = true;
    setState({});
    loader
      .current()
      .then((value) => active && setState({ value }))
      .catch(
        (error: unknown) =>
          active &&
          setState({
            error:
              error instanceof Error ? error.message : "Preview parsing failed",
          }),
      );
    return () => {
      active = false;
    };
  }, [cacheKey]);
  if (state.error)
    return (
      <PreviewNotice error>
        <strong>This preview could not be rendered.</strong>
        <p>{state.error}</p>
      </PreviewNotice>
    );
  if (state.value === undefined)
    return (
      <PreviewNotice>
        <Spinner size="small" /> Parsing this file in your browser…
      </PreviewNotice>
    );
  return children(state.value);
}

function OfficeFilePreview({
  bytes,
  name,
}: {
  bytes: Uint8Array;
  name: string;
}) {
  const [section, setSection] = useState(0);
  return (
    <AsyncValue
      cacheKey={name}
      load={() => parseOffice(bytes, extension(name))}
    >
      {(value: OfficePreview) => {
        if (value.kind === "workbook") {
          const selected =
            value.sheets[Math.min(section, value.sheets.length - 1)];
          return (
            <div className="office-preview">
              <nav className="preview-tabs" aria-label="Workbook sheets">
                {value.sheets.map((sheet, index) => (
                  <button
                    key={sheet.name}
                    className={index === section ? "active" : ""}
                    aria-current={index === section ? "page" : undefined}
                    onClick={() => setSection(index)}
                  >
                    {sheet.name}
                  </button>
                ))}
              </nav>
              {selected ? (
                <DataTable data={selected.table} name={selected.name} />
              ) : (
                <PreviewNotice>No worksheets were found.</PreviewNotice>
              )}
            </div>
          );
        }
        if (value.kind === "presentation")
          return (
            <div className="slide-deck-preview">
              {value.slides.map((slide, index) => (
                <section
                  className="slide-preview"
                  key={index}
                  aria-label={`Slide ${index + 1}`}
                >
                  <span>{index + 1}</span>
                  <h2>{slide.title}</h2>
                  {slide.lines.map((line, lineIndex) => (
                    <p key={lineIndex}>{line}</p>
                  ))}
                </section>
              ))}
            </div>
          );
        return (
          <article className="document-preview">
            {value.sections.map((item, index) =>
              item.table ? (
                <DataTable
                  key={index}
                  data={item.table}
                  name={`Table ${index + 1}`}
                />
              ) : (
                item.paragraphs.map((paragraph, paragraphIndex) => (
                  <p key={`${index}:${paragraphIndex}`}>{paragraph}</p>
                ))
              ),
            )}
          </article>
        );
      }}
    </AsyncValue>
  );
}

function DatabaseFilePreview({
  bytes,
  name,
}: {
  bytes: Uint8Array;
  name: string;
}) {
  const [selected, setSelected] = useState(0);
  return (
    <AsyncValue cacheKey={name} load={() => loadSqlite(bytes)}>
      {(value: DatabasePreview) => {
        const table = value.tables[Math.min(selected, value.tables.length - 1)];
        if (!table)
          return (
            <PreviewNotice>
              This database has no user tables or views.
            </PreviewNotice>
          );
        return (
          <div className="database-preview">
            <aside aria-label="Database objects">
              <strong>Database objects</strong>
              {value.tables.map((item, index) => (
                <button
                  key={item.name}
                  className={index === selected ? "active" : ""}
                  aria-current={index === selected ? "page" : undefined}
                  onClick={() => setSelected(index)}
                >
                  <span>{item.name}</span>
                  <small>{item.type}</small>
                </button>
              ))}
            </aside>
            <div className="database-table">
              <details>
                <summary>Schema for {table.name}</summary>
                <pre>{table.definition || "No stored schema statement"}</pre>
              </details>
              <DataTable data={table.data} name={table.name} />
            </div>
          </div>
        );
      }}
    </AsyncValue>
  );
}

function StructuredBinaryPreview({
  descriptor,
  bytes,
  name,
}: {
  descriptor: PreviewDescriptor;
  bytes: Uint8Array;
  name: string;
}) {
  if (descriptor.kind === "generic")
    return <GenericFilePreview bytes={bytes} name={name} />;
  if (descriptor.kind === "office")
    return <OfficeFilePreview bytes={bytes} name={name} />;
  if (descriptor.kind === "sqlite")
    return <DatabaseFilePreview bytes={bytes} name={name} />;
  if (descriptor.kind === "safetensors")
    try {
      return <DataTable data={parseSafetensors(bytes)} name="Model tensors" />;
    } catch (error) {
      return (
        <PreviewNotice error>
          {error instanceof Error ? error.message : "Invalid Safetensors file"}
        </PreviewNotice>
      );
    }
  if (descriptor.kind === "gguf")
    return (
      <AsyncValue cacheKey={name} load={() => loadGguf(bytes)}>
        {(value) => (
          <div className="stacked-data-preview">
            <DataTable data={value.metadata} name="Model metadata" />
            <DataTable data={value.tensors} name="Model tensors" />
          </div>
        )}
      </AsyncValue>
    );
  const loader =
    descriptor.kind === "parquet"
      ? () => loadParquet(bytes)
      : descriptor.kind === "arrow"
        ? () => loadArrow(bytes)
        : descriptor.kind === "numpy"
          ? () => loadNumpy(bytes, name)
          : () => archiveInventory(bytes, extension(name));
  return (
    <AsyncValue cacheKey={`${descriptor.kind}:${name}`} load={loader}>
      {(value) => <DataTable data={value} name={descriptor.label} />}
    </AsyncValue>
  );
}

function BinaryPreview({ descriptor, blobUrl, name, size, ...props }: Props) {
  const state = useBinary(blobUrl, size);
  if (state.error)
    return (
      <PreviewNotice error>
        <strong>Interactive preview unavailable.</strong>
        <p>{state.error}</p>
      </PreviewNotice>
    );
  if (state.loading || !state.bytes)
    return (
      <PreviewNotice>
        <Spinner size="small" /> Loading {formatBytes(size)} for a private
        in-browser preview…
      </PreviewNotice>
    );
  if (["markdown", "delimited", "json", "notebook"].includes(descriptor.kind)) {
    try {
      const text = new TextDecoder("utf-8", { fatal: true }).decode(
        state.bytes,
      );
      return (
        <FilePreview
          {...props}
          descriptor={descriptor}
          blobUrl={blobUrl}
          name={name}
          size={size}
          text={text}
        />
      );
    } catch {
      return (
        <PreviewNotice error>
          This file is not valid UTF-8 and cannot use the {descriptor.label}{" "}
          preview.
        </PreviewNotice>
      );
    }
  }
  if (["image", "pdf", "audio", "video"].includes(descriptor.kind))
    return (
      <MediaPreview descriptor={descriptor} bytes={state.bytes} name={name} />
    );
  return (
    <StructuredBinaryPreview
      descriptor={descriptor}
      bytes={state.bytes}
      name={name}
    />
  );
}

export function FilePreview(props: Props) {
  if (props.descriptor.kind === "markdown")
    return (
      <RepositoryMarkdown
        repo={props.repo}
        rev={props.rev}
        directory={props.directory}
        className="file-markdown-preview"
      >
        {props.text ?? ""}
      </RepositoryMarkdown>
    );
  if (props.descriptor.kind === "notebook")
    return (
      <NotebookPreview
        repo={props.repo}
        rev={props.rev}
        directory={props.directory}
        text={props.text}
      />
    );
  if (
    ["delimited", "json"].includes(props.descriptor.kind) &&
    props.text !== null
  )
    return (
      <TextDataPreview
        descriptor={props.descriptor}
        name={props.name}
        text={props.text}
      />
    );
  return <BinaryPreview {...props} />;
}
