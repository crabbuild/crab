// Wire Monaco's web workers to Vite-emitted asset URLs.
//
// Monaco loads language services (TypeScript, JSON, CSS, HTML) in
// separate Web Workers to keep the main thread responsive. Under
// Electron's `file://` runtime the default loader — which uses
// `blob:` URLs and `importScripts` — is blocked by Chromium's
// CORS policy, and every worker boot fails silently.
//
// The fix is to set `self.MonacoEnvironment.getWorker` to a factory
// that returns freshly-constructed `Worker` objects whose source is
// bundled by Vite. The `?worker` import suffix tells Vite to emit a
// standalone worker script and return a `Worker`-constructor class
// suitable for direct instantiation.

import EditorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";
import JsonWorker from "monaco-editor/esm/vs/language/json/json.worker?worker";
import CssWorker from "monaco-editor/esm/vs/language/css/css.worker?worker";
import HtmlWorker from "monaco-editor/esm/vs/language/html/html.worker?worker";
import TsWorker from "monaco-editor/esm/vs/language/typescript/ts.worker?worker";

// Set once — Monaco reads `self.MonacoEnvironment` on first editor
// creation. We assign via a cast because Monaco's bundled types
// define the property with a narrower shape than what we need here.
(self as unknown as { MonacoEnvironment: unknown }).MonacoEnvironment = {
  getWorker(_moduleId: string, label: string): Worker {
    switch (label) {
      case "json":
        return new JsonWorker();
      case "css":
      case "scss":
      case "less":
        return new CssWorker();
      case "html":
      case "handlebars":
      case "razor":
        return new HtmlWorker();
      case "typescript":
      case "javascript":
        return new TsWorker();
      default:
        return new EditorWorker();
    }
  },
};

// Initialize Shiki-powered syntax highlighting for Monaco.
// This runs async in the background — Monaco will use its built-in
// tokenizer until Shiki finishes loading, then seamlessly switches
// to the full TextMate grammar.
import { initShikiMonaco } from "./shiki-monaco";
initShikiMonaco();

// Configure @monaco-editor/react to use our local monaco-editor
// instance instead of loading from CDN. This ensures the MergeEditor
// (which uses @monaco-editor/react) shares the same Monaco instance
// that Shiki has configured.
import { loader } from "@monaco-editor/react";
import * as monaco from "monaco-editor";
loader.config({ monaco });

export {};
