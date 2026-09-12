import { Spinner } from "@primer/react";
import { useEffect, useRef, useState } from "react";
import type {
  PDFDocumentLoadingTask,
  PDFDocumentProxy,
  RenderTask,
} from "pdfjs-dist";
import pdfWorkerUrl from "pdfjs-dist/build/pdf.worker.min.mjs?url";

export function PdfPreview({
  bytes,
  name,
}: {
  bytes: Uint8Array;
  name: string;
}) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const [document, setDocument] = useState<PDFDocumentProxy>();
  const [page, setPage] = useState(1);
  const [zoom, setZoom] = useState(125);
  const [pageText, setPageText] = useState("");
  const [error, setError] = useState<string>();

  useEffect(() => {
    let active = true;
    let loading: PDFDocumentLoadingTask | undefined;
    void import("pdfjs-dist")
      .then(async ({ getDocument, GlobalWorkerOptions }) => {
        // The query distinguishes the executable worker response from any
        // previously cached binary-media response for the same content hash.
        GlobalWorkerOptions.workerSrc = `${pdfWorkerUrl}?module=1`;
        loading = getDocument({ data: bytes.slice() });
        const loaded = await loading.promise;
        if (active) setDocument(loaded);
        else await loading.destroy();
      })
      .catch((reason: unknown) => {
        if (active)
          setError(
            reason instanceof Error ? reason.message : "PDF loading failed",
          );
      });
    return () => {
      active = false;
      if (loading) void loading.destroy();
    };
  }, [bytes]);

  useEffect(() => {
    if (!document || !canvas.current) return;
    let active = true;
    let rendering: RenderTask | undefined;
    setPageText("");
    void document
      .getPage(page)
      .then(async (pdfPage) => {
        if (!active || !canvas.current) return;
        const viewport = pdfPage.getViewport({ scale: zoom / 100 });
        const context = canvas.current.getContext("2d");
        if (!context) throw new Error("Canvas rendering is unavailable");
        const ratio = Math.min(window.devicePixelRatio || 1, 2);
        canvas.current.width = Math.floor(viewport.width * ratio);
        canvas.current.height = Math.floor(viewport.height * ratio);
        canvas.current.style.width = `${Math.floor(viewport.width)}px`;
        canvas.current.style.height = `${Math.floor(viewport.height)}px`;
        rendering = pdfPage.render({
          canvas: canvas.current,
          canvasContext: context,
          transform: ratio === 1 ? undefined : [ratio, 0, 0, ratio, 0, 0],
          viewport,
        });
        const text = await pdfPage.getTextContent();
        if (active)
          setPageText(
            text.items.map((item) => ("str" in item ? item.str : "")).join(" "),
          );
        await rendering.promise;
      })
      .catch((reason: unknown) => {
        if (
          active &&
          !(
            reason instanceof Error &&
            reason.name === "RenderingCancelledException"
          )
        )
          setError(
            reason instanceof Error ? reason.message : "PDF rendering failed",
          );
      });
    return () => {
      active = false;
      rendering?.cancel();
    };
  }, [document, page, zoom]);

  if (error)
    return (
      <div className="file-preview-notice error" role="alert">
        <div>
          <strong>This PDF could not be rendered.</strong>
          <p>{error}</p>
        </div>
      </div>
    );
  if (!document)
    return (
      <div className="file-preview-notice" role="status">
        <Spinner size="small" /> Preparing this PDF in your browser…
      </div>
    );
  return (
    <div className="pdf-preview">
      <div className="pdf-preview-toolbar">
        <div className="pdf-page-controls">
          <button
            className="button-link"
            disabled={page === 1}
            onClick={() => setPage((value) => Math.max(1, value - 1))}
          >
            Previous
          </button>
          <span>
            Page {page} of {document.numPages}
          </span>
          <button
            className="button-link"
            disabled={page === document.numPages}
            onClick={() =>
              setPage((value) => Math.min(document.numPages, value + 1))
            }
          >
            Next
          </button>
        </div>
        <label>
          Zoom
          <input
            aria-label="PDF zoom"
            type="range"
            min="50"
            max="200"
            step="25"
            value={zoom}
            onChange={(event) => setZoom(Number(event.target.value))}
          />
          <output>{zoom}%</output>
        </label>
      </div>
      <div className="pdf-page-stage" tabIndex={0}>
        <canvas
          ref={canvas}
          role="img"
          aria-label={`Page ${page} of ${name}`}
        />
        <span className="sr-only">{pageText}</span>
      </div>
    </div>
  );
}
