/**
 * Renderer-side LSP client that registers Monaco providers.
 *
 * Takes an LspConnection + ServerCapabilities and conditionally registers
 * completion, hover, diagnostics, and other providers based on what the
 * server supports.
 */

import * as monaco from "monaco-editor";
import { LspConnection } from "./lsp-connection";
import {
  type LspServerCapabilities,
  type LspCompletionItem,
  type LspCompletionList,
  type LspHover,
  type LspPublishDiagnosticsParams,
  type LspLocation,
  type LspLocationLink,
  type LspCodeAction,
  type LspWorkspaceEdit,
  type LspSignatureHelp,
  type LspPrepareRenameResult,
  type LspTextEdit,
  type LspRange,
  type LspInlayHint,
  type LspDocumentHighlight,
  type LspMarkupContent,
  type LspSelectionRange,
  type LspColorInformation,
  type LspColorPresentation,
  type LspCodeLens,
  type LspCallHierarchyItem,
  type LspCallHierarchyIncomingCall,
  type LspCallHierarchyOutgoingCall,
  LspInlayHintKind,
  LspDocumentHighlightKind,
} from "./lsp-types";
import {
  monacoToLspPosition,
  monacoToLspRange,
  lspToMonacoRange,
  lspCompletionItemToMonaco,
  lspMarkupToMonaco,
  lspHoverToMonacoContents,
  lspDiagnosticToMonacoMarker,
  lspDefinitionResultToMonaco,
  lspSignatureHelpToMonaco,
  lspTextEditToMonaco,
} from "./lsp-converters";

// ── LspClient ───────────────────────────────────────────────────────

export class LspClient {
  private connection: LspConnection;
  private disposables: monaco.IDisposable[] = [];
  private openDocuments = new Set<string>();
  private documentVersions = new Map<string, number>();
  private diagnosticUnsubscribe: (() => void) | null = null;

  /** Callback invoked when go-to-definition navigates to a different file. */
  onDefinitionNavigation: ((uri: string, line: number) => void) | null = null;

  constructor(
    private language: string,
    private capabilities: LspServerCapabilities,
    connection?: LspConnection,
  ) {
    this.connection = connection ?? new LspConnection(language);
    this.registerProviders();
    this.registerDiagnostics();
  }

  /** Server capabilities negotiated during `initialize`. */
  getCapabilities(): LspServerCapabilities {
    return this.capabilities;
  }

  // ── Document Synchronization ────────────────────────────────────

  /** Notify server that a document was opened. */
  documentOpened(uri: string, languageId: string, version: number, text: string): void {
    this.openDocuments.add(uri);
    this.documentVersions.set(uri, version);
    this.connection.notify("textDocument/didOpen", {
      textDocument: { uri, languageId, version, text },
    });
  }

  /** Notify server of document content change (full sync). */
  documentChanged(uri: string, version: number, text: string): void {
    this.documentVersions.set(uri, version);
    this.connection.notify("textDocument/didChange", {
      textDocument: { uri, version },
      contentChanges: [{ text }],
    });
  }

  /** Notify server that a document was closed. */
  documentClosed(uri: string): void {
    this.openDocuments.delete(uri);
    this.documentVersions.delete(uri);
    this.connection.notify("textDocument/didClose", {
      textDocument: { uri },
    });

    // Clear markers for this file
    const model = monaco.editor.getModels().find((m) => m.uri.toString() === uri);
    if (model) {
      monaco.editor.setModelMarkers(model, "lsp", []);
    }
  }

  /** Notify server that a document was saved. */
  documentSaved(uri: string, text?: string): void {
    const params: { textDocument: { uri: string }; text?: string } = {
      textDocument: { uri },
    };
    if (text != null) params.text = text;
    this.connection.notify("textDocument/didSave", params);
  }

  /** Get the underlying connection (for testing or advanced use). */
  getConnection(): LspConnection {
    return this.connection;
  }

  /** Dispose all providers and the connection. */
  dispose(): void {
    for (const d of this.disposables) d.dispose();
    this.disposables = [];

    if (this.diagnosticUnsubscribe) {
      this.diagnosticUnsubscribe();
      this.diagnosticUnsubscribe = null;
    }

    // Close all open documents
    for (const uri of this.openDocuments) {
      this.connection.notify("textDocument/didClose", { textDocument: { uri } });
    }
    this.openDocuments.clear();
    this.documentVersions.clear();

    this.connection.dispose();
  }

  // ── Provider Registration ───────────────────────────────────────

  private registerProviders(): void {
    if (this.capabilities.completionProvider) {
      this.disposables.push(
        monaco.languages.registerCompletionItemProvider(this.language, this.createCompletionProvider()),
      );
    }

    if (this.capabilities.hoverProvider) {
      this.disposables.push(
        monaco.languages.registerHoverProvider(this.language, this.createHoverProvider()),
      );
    }

    if (this.capabilities.definitionProvider) {
      this.disposables.push(
        monaco.languages.registerDefinitionProvider(this.language, this.createDefinitionProvider()),
      );
    }

    if (this.capabilities.typeDefinitionProvider) {
      this.disposables.push(
        monaco.languages.registerTypeDefinitionProvider(this.language, this.createTypeDefinitionProvider()),
      );
    }

    if (this.capabilities.referencesProvider) {
      this.disposables.push(
        monaco.languages.registerReferenceProvider(this.language, this.createReferenceProvider()),
      );
    }

    if (this.capabilities.codeActionProvider) {
      this.disposables.push(
        monaco.languages.registerCodeActionProvider(this.language, this.createCodeActionProvider()),
      );
    }

    if (this.capabilities.renameProvider) {
      this.disposables.push(
        monaco.languages.registerRenameProvider(this.language, this.createRenameProvider()),
      );
    }

    if (this.capabilities.documentFormattingProvider) {
      this.disposables.push(
        monaco.languages.registerDocumentFormattingEditProvider(this.language, this.createFormattingProvider()),
      );
    }

    if (this.capabilities.documentRangeFormattingProvider) {
      this.disposables.push(
        monaco.languages.registerDocumentRangeFormattingEditProvider(this.language, this.createRangeFormattingProvider()),
      );
    }

    if (this.capabilities.signatureHelpProvider) {
      this.disposables.push(
        monaco.languages.registerSignatureHelpProvider(this.language, this.createSignatureHelpProvider()),
      );
    }

    if (this.capabilities.implementationProvider) {
      this.disposables.push(
        monaco.languages.registerImplementationProvider(this.language, this.createImplementationProvider()),
      );
    }

    if (this.capabilities.inlayHintProvider) {
      this.disposables.push(
        monaco.languages.registerInlayHintsProvider(this.language, this.createInlayHintsProvider()),
      );
    }

    if (this.capabilities.documentHighlightProvider) {
      this.disposables.push(
        monaco.languages.registerDocumentHighlightProvider(this.language, this.createDocumentHighlightProvider()),
      );
    }

    if (this.capabilities.selectionRangeProvider) {
      this.disposables.push(
        monaco.languages.registerSelectionRangeProvider(this.language, this.createSelectionRangeProvider()),
      );
    }

    if (this.capabilities.colorProvider) {
      this.disposables.push(
        monaco.languages.registerColorProvider(this.language, this.createColorProvider()),
      );
    }

    if (this.capabilities.codeLensProvider) {
      this.disposables.push(
        monaco.languages.registerCodeLensProvider(this.language, this.createCodeLensProvider()),
      );
    }
  }

  // ── Completions (Task 8) ────────────────────────────────────────

  private createCompletionProvider(): monaco.languages.CompletionItemProvider {
    const connection = this.connection;
    const capabilities = this.capabilities;
    let lastRequestTime = 0;

    const triggerCharacters = (capabilities.completionProvider as { triggerCharacters?: string[] })?.triggerCharacters;
    const resolveProvider = (capabilities.completionProvider as { resolveProvider?: boolean })?.resolveProvider;

    const provider: monaco.languages.CompletionItemProvider = {
      triggerCharacters,

      provideCompletionItems: async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        context: monaco.languages.CompletionContext,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.CompletionList | null> => {
        // Debounce: don't send more than once per 100ms
        const now = Date.now();
        if (now - lastRequestTime < 100) {
          return null;
        }
        lastRequestTime = now;

        const lspPosition = monacoToLspPosition(position);
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/completion", {
            textDocument: { uri },
            position: lspPosition,
            context: {
              triggerKind: context.triggerKind,
              triggerCharacter: context.triggerCharacter,
            },
          }, 2000);

          if (!result) return null;

          const items: LspCompletionItem[] = Array.isArray(result)
            ? result
            : (result as LspCompletionList).items ?? [];

          const isIncomplete = !Array.isArray(result) && (result as LspCompletionList).isIncomplete;

          // Default range: the word at the current position
          const word = model.getWordUntilPosition(position);
          const defaultRange: monaco.IRange = {
            startLineNumber: position.lineNumber,
            startColumn: word.startColumn,
            endLineNumber: position.lineNumber,
            endColumn: word.endColumn,
          };

          const monacoItems = items.map((item) => lspCompletionItemToMonaco(item, defaultRange));

          return {
            suggestions: monacoItems as unknown as monaco.languages.CompletionItem[],
            incomplete: isIncomplete ?? false,
          };
        } catch {
          return null;
        }
      },
    };

    // completionItem/resolve for lazy documentation loading
    if (resolveProvider) {
      provider.resolveCompletionItem = async (
        item: monaco.languages.CompletionItem,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.CompletionItem> => {
        const lspItem = (item as unknown as { _lspItem?: LspCompletionItem })._lspItem;
        if (!lspItem) return item;

        try {
          const resolved = await connection.request("completionItem/resolve", lspItem, 2000) as LspCompletionItem;

          if (resolved.documentation) {
            if (typeof resolved.documentation === "string") {
              (item as any).documentation = { value: resolved.documentation };
            } else {
              (item as any).documentation = lspMarkupToMonaco(resolved.documentation);
            }
          }
          if (resolved.detail) {
            item.detail = resolved.detail;
          }
          if (resolved.additionalTextEdits) {
            (item as any).additionalTextEdits = resolved.additionalTextEdits.map((edit) => ({
              range: lspToMonacoRange(edit.range),
              text: edit.newText,
            }));
          }
        } catch {
          // Resolve failed — return item as-is
        }

        return item;
      };
    }

    return provider;
  }

  // ── Hover (Task 10) ─────────────────────────────────────────────

  private createHoverProvider(): monaco.languages.HoverProvider {
    const connection = this.connection;

    return {
      provideHover: async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.Hover | null> => {
        const lspPosition = monacoToLspPosition(position);
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/hover", {
            textDocument: { uri },
            position: lspPosition,
          }, 5000) as LspHover | null;

          if (!result || !result.contents) return null;

          const contents = lspHoverToMonacoContents(result);
          if (contents.length === 0) return null;

          const hover: monaco.languages.Hover = {
            contents: contents as monaco.IMarkdownString[],
          };

          if (result.range) {
            hover.range = lspToMonacoRange(result.range);
          }

          return hover;
        } catch {
          return null;
        }
      },
    };
  }

  // ── Diagnostics (Task 9) ────────────────────────────────────────

  private registerDiagnostics(): void {
    this.diagnosticUnsubscribe = this.connection.onNotification(
      "textDocument/publishDiagnostics",
      (params: unknown) => {
        const diagParams = params as LspPublishDiagnosticsParams;
        if (!diagParams?.uri) return;

        const uri = diagParams.uri;
        const model = monaco.editor.getModels().find((m) => m.uri.toString() === uri);
        if (!model) return;

        if (!diagParams.diagnostics || diagParams.diagnostics.length === 0) {
          // Clear markers when server sends empty diagnostics
          monaco.editor.setModelMarkers(model, "lsp", []);
          return;
        }

        const markers = diagParams.diagnostics.map((d) => lspDiagnosticToMonacoMarker(d));
        monaco.editor.setModelMarkers(model, "lsp", markers as unknown as monaco.editor.IMarkerData[]);
      },
    );
  }

  // ── Definition (Task 14) ────────────────────────────────────────

  private createDefinitionProvider(): monaco.languages.DefinitionProvider {
    const connection = this.connection;
    const client = this;

    return {
      provideDefinition: async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.Definition | null> => {
        const lspPosition = monacoToLspPosition(position);
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/definition", {
            textDocument: { uri },
            position: lspPosition,
          }, 5000) as LspLocation | LspLocation[] | LspLocationLink[] | null;

          const locations = lspDefinitionResultToMonaco(result);
          if (locations.length === 0) return null;

          // If definition is in a different file, notify for cross-file navigation
          for (const loc of locations) {
            if (loc.uri !== uri && client.onDefinitionNavigation) {
              client.onDefinitionNavigation(loc.uri, loc.range.startLineNumber);
            }
          }

          return locations.map((loc) => ({
            uri: monaco.Uri.parse(loc.uri),
            range: loc.range,
          })) as monaco.languages.Definition;
        } catch {
          return null;
        }
      },
    };
  }

  // ── Type Definition (Task 14.5) ─────────────────────────────────

  private createTypeDefinitionProvider(): monaco.languages.TypeDefinitionProvider {
    const connection = this.connection;
    const client = this;

    return {
      provideTypeDefinition: async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.Definition | null> => {
        const lspPosition = monacoToLspPosition(position);
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/typeDefinition", {
            textDocument: { uri },
            position: lspPosition,
          }, 5000) as LspLocation | LspLocation[] | LspLocationLink[] | null;

          const locations = lspDefinitionResultToMonaco(result);
          if (locations.length === 0) return null;

          // Cross-file navigation callback
          for (const loc of locations) {
            if (loc.uri !== uri && client.onDefinitionNavigation) {
              client.onDefinitionNavigation(loc.uri, loc.range.startLineNumber);
            }
          }

          return locations.map((loc) => ({
            uri: monaco.Uri.parse(loc.uri),
            range: loc.range,
          })) as monaco.languages.Definition;
        } catch {
          return null;
        }
      },
    };
  }

  // ── References (Task 15) ────────────────────────────────────────

  private createReferenceProvider(): monaco.languages.ReferenceProvider {
    const connection = this.connection;

    return {
      provideReferences: async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        context: monaco.languages.ReferenceContext,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.Location[] | null> => {
        const lspPosition = monacoToLspPosition(position);
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/references", {
            textDocument: { uri },
            position: lspPosition,
            context: {
              includeDeclaration: context.includeDeclaration,
            },
          }, 10000) as LspLocation[] | null;

          if (!result || result.length === 0) return null;

          return result.map((loc) => ({
            uri: monaco.Uri.parse(loc.uri),
            range: lspToMonacoRange(loc.range),
          }));
        } catch {
          return null;
        }
      },
    };
  }

  // ── Code Actions (Task 16) ──────────────────────────────────────

  private createCodeActionProvider(): monaco.languages.CodeActionProvider {
    const connection = this.connection;
    const capabilities = this.capabilities;
    const resolveSupported = typeof capabilities.codeActionProvider === "object" &&
      (capabilities.codeActionProvider as { resolveProvider?: boolean }).resolveProvider;

    return {
      provideCodeActions: async (
        model: monaco.editor.ITextModel,
        range: monaco.Range,
        context: monaco.languages.CodeActionContext,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.CodeActionList | null> => {
        const uri = model.uri.toString();
        const lspRange = monacoToLspRange(range);

        // Build diagnostics context from Monaco markers in the range
        const markers = monaco.editor.getModelMarkers({ resource: model.uri });
        const diagnosticsInRange = markers
          .filter((m) =>
            m.startLineNumber >= range.startLineNumber &&
            m.endLineNumber <= range.endLineNumber,
          )
          .map((m) => ({
            range: {
              start: { line: m.startLineNumber - 1, character: m.startColumn - 1 },
              end: { line: m.endLineNumber - 1, character: m.endColumn - 1 },
            },
            severity: monacoSeverityToLsp(m.severity),
            message: m.message,
            source: m.source,
            code: typeof m.code === "object" ? m.code.value : m.code,
          }));

        try {
          const result = await connection.request("textDocument/codeAction", {
            textDocument: { uri },
            range: lspRange,
            context: {
              diagnostics: diagnosticsInRange,
              only: context.only ? [context.only] : undefined,
            },
          }, 5000) as (LspCodeAction | { title: string })[] | null;

          if (!result || result.length === 0) return null;

          const actions: monaco.languages.CodeAction[] = result
            .filter((item): item is LspCodeAction => "title" in item)
            .map((action) => {
              const monacoAction: monaco.languages.CodeAction = {
                title: action.title,
                kind: action.kind,
                isPreferred: action.isPreferred,
                disabled: action.disabled ? action.disabled.reason : undefined,
              };

              if (action.edit) {
                monacoAction.edit = lspWorkspaceEditToMonaco(action.edit);
              }

              // Stash the original action for resolve
              (monacoAction as any)._lspCodeAction = action;

              return monacoAction;
            });

          return {
            actions,
            dispose: () => {},
          };
        } catch {
          return null;
        }
      },

      resolveCodeAction: resolveSupported
        ? async (
            codeAction: monaco.languages.CodeAction,
            _token: monaco.CancellationToken,
          ): Promise<monaco.languages.CodeAction> => {
            const lspAction = (codeAction as any)._lspCodeAction as LspCodeAction | undefined;
            if (!lspAction) return codeAction;

            try {
              const resolved = await connection.request(
                "codeAction/resolve",
                lspAction,
                5000,
              ) as LspCodeAction;

              if (resolved.edit) {
                codeAction.edit = lspWorkspaceEditToMonaco(resolved.edit);
              }
            } catch {
              // Resolve failed — return action as-is
            }

            return codeAction;
          }
        : undefined,
    };
  }

  // ── Rename (Task 17) ────────────────────────────────────────────

  private createRenameProvider(): monaco.languages.RenameProvider {
    const connection = this.connection;
    const capabilities = this.capabilities;
    const hasPrepare = typeof capabilities.renameProvider === "object" &&
      (capabilities.renameProvider as { prepareProvider?: boolean }).prepareProvider;

    const provider: monaco.languages.RenameProvider = {
      provideRenameEdits: async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        newName: string,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.WorkspaceEdit | null> => {
        const lspPosition = monacoToLspPosition(position);
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/rename", {
            textDocument: { uri },
            position: lspPosition,
            newName,
          }, 10000) as LspWorkspaceEdit | null;

          if (!result) return null;

          return lspWorkspaceEditToMonaco(result);
        } catch {
          return null;
        }
      },
    };

    if (hasPrepare) {
      provider.resolveRenameLocation = async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.RenameLocation | null> => {
        const lspPosition = monacoToLspPosition(position);
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/prepareRename", {
            textDocument: { uri },
            position: lspPosition,
          }, 5000) as LspPrepareRenameResult | LspRange | null;

          if (!result) {
            return { range: new monaco.Range(0, 0, 0, 0), text: "" };
          }

          // Handle different response formats
          if ("placeholder" in (result as LspPrepareRenameResult)) {
            const prepared = result as LspPrepareRenameResult;
            return {
              range: lspToMonacoRange(prepared.range) as unknown as monaco.Range,
              text: prepared.placeholder,
            };
          }

          // Plain range response — use the text at that range as placeholder
          const range = lspToMonacoRange(result as LspRange);
          const text = model.getValueInRange(range);
          return {
            range: range as unknown as monaco.Range,
            text,
          };
        } catch {
          return { range: new monaco.Range(0, 0, 0, 0), text: "" };
        }
      };
    }

    return provider;
  }

  // ── Formatting (Task 18) ────────────────────────────────────────

  private createFormattingProvider(): monaco.languages.DocumentFormattingEditProvider {
    const connection = this.connection;

    return {
      provideDocumentFormattingEdits: async (
        model: monaco.editor.ITextModel,
        options: monaco.languages.FormattingOptions,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.TextEdit[] | null> => {
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/formatting", {
            textDocument: { uri },
            options: {
              tabSize: options.tabSize,
              insertSpaces: options.insertSpaces,
              trimTrailingWhitespace: true,
              insertFinalNewline: true,
              trimFinalNewlines: true,
            },
          }, 10000) as LspTextEdit[] | null;

          if (!result || result.length === 0) return null;

          return result.map((edit) => lspTextEditToMonaco(edit)) as monaco.languages.TextEdit[];
        } catch {
          return null;
        }
      },
    };
  }

  private createRangeFormattingProvider(): monaco.languages.DocumentRangeFormattingEditProvider {
    const connection = this.connection;

    return {
      provideDocumentRangeFormattingEdits: async (
        model: monaco.editor.ITextModel,
        range: monaco.Range,
        options: monaco.languages.FormattingOptions,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.TextEdit[] | null> => {
        const uri = model.uri.toString();
        const lspRange = monacoToLspRange(range);

        try {
          const result = await connection.request("textDocument/rangeFormatting", {
            textDocument: { uri },
            range: lspRange,
            options: {
              tabSize: options.tabSize,
              insertSpaces: options.insertSpaces,
              trimTrailingWhitespace: true,
              insertFinalNewline: true,
              trimFinalNewlines: true,
            },
          }, 10000) as LspTextEdit[] | null;

          if (!result || result.length === 0) return null;

          return result.map((edit) => lspTextEditToMonaco(edit)) as monaco.languages.TextEdit[];
        } catch {
          return null;
        }
      },
    };
  }

  // ── Signature Help (Task 19) ────────────────────────────────────

  private createSignatureHelpProvider(): monaco.languages.SignatureHelpProvider {
    const connection = this.connection;
    const sigOptions = this.capabilities.signatureHelpProvider;

    const triggerCharacters = sigOptions?.triggerCharacters ?? ["(", ","];
    const retriggerCharacters = sigOptions?.retriggerCharacters ?? [",", ")"];

    return {
      signatureHelpTriggerCharacters: triggerCharacters,
      signatureHelpRetriggerCharacters: retriggerCharacters,

      provideSignatureHelp: async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        _token: monaco.CancellationToken,
        context: monaco.languages.SignatureHelpContext,
      ): Promise<monaco.languages.SignatureHelpResult | null> => {
        const lspPosition = monacoToLspPosition(position);
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/signatureHelp", {
            textDocument: { uri },
            position: lspPosition,
            context: {
              triggerKind: context.triggerKind,
              triggerCharacter: context.triggerCharacter,
              isRetrigger: context.isRetrigger,
              activeSignatureHelp: context.activeSignatureHelp
                ? {
                    signatures: [],
                    activeSignature: context.activeSignatureHelp.activeSignature,
                    activeParameter: context.activeSignatureHelp.activeParameter,
                  }
                : undefined,
            },
          }, 5000) as LspSignatureHelp | null;

          if (!result || !result.signatures || result.signatures.length === 0) return null;

          const monacoHelp = lspSignatureHelpToMonaco(result);

          return {
            value: monacoHelp as unknown as monaco.languages.SignatureHelp,
            dispose: () => {},
          };
        } catch {
          return null;
        }
      },
    };
  }

  // ── Implementation ──────────────────────────────────────────────

  private createImplementationProvider(): monaco.languages.ImplementationProvider {
    const connection = this.connection;

    return {
      provideImplementation: async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.Definition | null> => {
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/implementation", {
            textDocument: { uri },
            position: monacoToLspPosition(position),
          }, 5000) as LspLocation | LspLocation[] | LspLocationLink[] | null;

          const locations = lspDefinitionResultToMonaco(result);
          if (locations.length === 0) return null;

          return locations.map((loc) => ({
            uri: monaco.Uri.parse(loc.uri),
            range: loc.range,
          })) as monaco.languages.Definition;
        } catch {
          return null;
        }
      },
    };
  }

  // ── Inlay Hints ─────────────────────────────────────────────────

  private createInlayHintsProvider(): monaco.languages.InlayHintsProvider {
    const connection = this.connection;

    return {
      provideInlayHints: async (
        model: monaco.editor.ITextModel,
        range: monaco.Range,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.InlayHintList | null> => {
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/inlayHint", {
            textDocument: { uri },
            range: monacoToLspRange(range),
          }, 5000) as LspInlayHint[] | null;

          if (!result || result.length === 0) {
            return { hints: [], dispose: () => {} };
          }

          const hints: monaco.languages.InlayHint[] = result.map((hint) => ({
            position: {
              lineNumber: hint.position.line + 1,
              column: hint.position.character + 1,
            },
            label: inlayLabelForMonaco(hint.label),
            kind: mapInlayHintKind(hint.kind),
            paddingLeft: hint.paddingLeft,
            paddingRight: hint.paddingRight,
            tooltip: hint.tooltip
              ? typeof hint.tooltip === "string"
                ? hint.tooltip
                : lspMarkupToMonaco(hint.tooltip as LspMarkupContent) as monaco.IMarkdownString
              : undefined,
          }));

          return { hints, dispose: () => {} };
        } catch {
          return { hints: [], dispose: () => {} };
        }
      },
    };
  }

  // ── Document Highlight ──────────────────────────────────────────

  private createDocumentHighlightProvider(): monaco.languages.DocumentHighlightProvider {
    const connection = this.connection;

    return {
      provideDocumentHighlights: async (
        model: monaco.editor.ITextModel,
        position: monaco.Position,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.DocumentHighlight[] | null> => {
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/documentHighlight", {
            textDocument: { uri },
            position: monacoToLspPosition(position),
          }, 2000) as LspDocumentHighlight[] | null;

          if (!result || result.length === 0) return null;

          return result.map((h) => ({
            range: lspToMonacoRange(h.range) as monaco.IRange,
            kind: mapDocumentHighlightKind(h.kind),
          })) as monaco.languages.DocumentHighlight[];
        } catch {
          return null;
        }
      },
    };
  }

  // ── Selection Range (semantic Opt+Shift+↑ expansion) ────────────

  private createSelectionRangeProvider(): monaco.languages.SelectionRangeProvider {
    const connection = this.connection;

    return {
      provideSelectionRanges: async (
        model: monaco.editor.ITextModel,
        positions: monaco.Position[],
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.SelectionRange[][] | null> => {
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/selectionRange", {
            textDocument: { uri },
            positions: positions.map(monacoToLspPosition),
          }, 2000) as LspSelectionRange[] | null;

          if (!result || result.length === 0) return null;

          // Convert each position's linked-list of parent ranges into
          // a flat array, as Monaco expects.
          return result.map((top) => {
            const ranges: monaco.languages.SelectionRange[] = [];
            let node: LspSelectionRange | undefined = top;
            while (node) {
              ranges.push({ range: lspToMonacoRange(node.range) as monaco.IRange });
              node = node.parent;
            }
            return ranges;
          });
        } catch {
          return null;
        }
      },
    };
  }

  // ── Document Color (CSS / Tailwind color swatches) ──────────────

  private createColorProvider(): monaco.languages.DocumentColorProvider {
    const connection = this.connection;

    return {
      provideDocumentColors: async (
        model: monaco.editor.ITextModel,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.IColorInformation[]> => {
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/documentColor", {
            textDocument: { uri },
          }, 3000) as LspColorInformation[] | null;

          if (!result) return [];

          return result.map((info) => ({
            range: lspToMonacoRange(info.range) as monaco.IRange,
            color: {
              red: info.color.red,
              green: info.color.green,
              blue: info.color.blue,
              alpha: info.color.alpha,
            },
          }));
        } catch {
          return [];
        }
      },

      provideColorPresentations: async (
        model: monaco.editor.ITextModel,
        colorInfo: monaco.languages.IColorInformation,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.IColorPresentation[]> => {
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/colorPresentation", {
            textDocument: { uri },
            color: colorInfo.color,
            range: monacoToLspRange(colorInfo.range),
          }, 3000) as LspColorPresentation[] | null;

          if (!result) return [];

          return result.map((presentation) => ({
            label: presentation.label,
            textEdit: presentation.textEdit
              ? {
                  range: lspToMonacoRange(presentation.textEdit.range) as monaco.IRange,
                  text: presentation.textEdit.newText,
                }
              : undefined,
            additionalTextEdits: presentation.additionalTextEdits?.map((edit) => ({
              range: lspToMonacoRange(edit.range) as monaco.IRange,
              text: edit.newText,
            })),
          }));
        } catch {
          return [];
        }
      },
    };
  }

  // ── Code Lens (inline reference counts, run buttons, etc.) ──────

  private createCodeLensProvider(): monaco.languages.CodeLensProvider {
    const connection = this.connection;
    const resolveProvider = typeof this.capabilities.codeLensProvider === "object" &&
      (this.capabilities.codeLensProvider as { resolveProvider?: boolean }).resolveProvider;

    const provider: monaco.languages.CodeLensProvider = {
      provideCodeLenses: async (
        model: monaco.editor.ITextModel,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.CodeLensList | null> => {
        const uri = model.uri.toString();

        try {
          const result = await connection.request("textDocument/codeLens", {
            textDocument: { uri },
          }, 5000) as LspCodeLens[] | null;

          if (!result || result.length === 0) {
            return { lenses: [], dispose: () => {} };
          }

          const lenses: monaco.languages.CodeLens[] = result.map((lens, idx) => ({
            range: lspToMonacoRange(lens.range) as monaco.IRange,
            id: `lsp-lens-${idx}`,
            command: lens.command
              ? {
                  id: lens.command.command,
                  title: lens.command.title,
                  arguments: lens.command.arguments,
                }
              : undefined,
          }));

          // Stash the original LSP lenses alongside for resolve
          for (let i = 0; i < lenses.length; i++) {
            (lenses[i] as monaco.languages.CodeLens & { _lspLens?: LspCodeLens })._lspLens = result[i];
          }

          return { lenses, dispose: () => {} };
        } catch {
          return { lenses: [], dispose: () => {} };
        }
      },
    };

    if (resolveProvider) {
      provider.resolveCodeLens = async (
        _model: monaco.editor.ITextModel,
        codeLens: monaco.languages.CodeLens,
        _token: monaco.CancellationToken,
      ): Promise<monaco.languages.CodeLens> => {
        const lspLens = (codeLens as monaco.languages.CodeLens & { _lspLens?: LspCodeLens })._lspLens;
        if (!lspLens) return codeLens;

        try {
          const resolved = await connection.request(
            "codeLens/resolve",
            lspLens,
            3000,
          ) as LspCodeLens;

          if (resolved.command) {
            codeLens.command = {
              id: resolved.command.command,
              title: resolved.command.title,
              arguments: resolved.command.arguments,
            };
          }
        } catch {
          // Leave as-is on error
        }

        return codeLens;
      };
    }

    return provider;
  }

  // ── Call Hierarchy (incoming / outgoing calls) ──────────────────

  /**
   * Prepare a call hierarchy at a position. Returns the items that
   * can serve as roots; typically a single function/method.
   */
  async prepareCallHierarchy(
    uri: string,
    line: number,
    character: number,
  ): Promise<LspCallHierarchyItem[] | null> {
    if (!this.capabilities.callHierarchyProvider) return null;
    try {
      const result = await this.connection.request(
        "textDocument/prepareCallHierarchy",
        {
          textDocument: { uri },
          position: { line, character },
        },
        5000,
      ) as LspCallHierarchyItem[] | null;
      return result;
    } catch {
      return null;
    }
  }

  /** Fetch callers of an item (incoming calls). */
  async callHierarchyIncoming(item: LspCallHierarchyItem): Promise<LspCallHierarchyIncomingCall[]> {
    if (!this.capabilities.callHierarchyProvider) return [];
    try {
      const result = await this.connection.request(
        "callHierarchy/incomingCalls",
        { item },
        5000,
      ) as LspCallHierarchyIncomingCall[] | null;
      return result ?? [];
    } catch {
      return [];
    }
  }

  /** Fetch callees of an item (outgoing calls). */
  async callHierarchyOutgoing(item: LspCallHierarchyItem): Promise<LspCallHierarchyOutgoingCall[]> {
    if (!this.capabilities.callHierarchyProvider) return [];
    try {
      const result = await this.connection.request(
        "callHierarchy/outgoingCalls",
        { item },
        5000,
      ) as LspCallHierarchyOutgoingCall[] | null;
      return result ?? [];
    } catch {
      return [];
    }
  }
}

// ── Inlay hint helpers ────────────────────────────────────────────────

function inlayLabelForMonaco(
  label: LspInlayHint["label"],
): string | monaco.languages.InlayHintLabelPart[] {
  if (typeof label === "string") return label;
  // Array of label parts → Monaco's InlayHintLabelPart[]
  return label.map((part) => ({
    label: part.value,
    tooltip: part.tooltip
      ? typeof part.tooltip === "string"
        ? part.tooltip
        : (lspMarkupToMonaco(part.tooltip as LspMarkupContent) as monaco.IMarkdownString)
      : undefined,
  }));
}

function mapInlayHintKind(
  kind: LspInlayHintKind | undefined,
): monaco.languages.InlayHintKind | undefined {
  switch (kind) {
    case LspInlayHintKind.Type:
      return 1;
    case LspInlayHintKind.Parameter:
      return 2;
    default:
      return undefined;
  }
}

function mapDocumentHighlightKind(
  kind: LspDocumentHighlightKind | undefined,
): monaco.languages.DocumentHighlightKind {
  switch (kind) {
    case LspDocumentHighlightKind.Write:
      return 2;
    case LspDocumentHighlightKind.Read:
      return 1;
    case LspDocumentHighlightKind.Text:
    default:
      return 0;
  }
}

// ── Helper Functions ──────────────────────────────────────────────────

/** Map Monaco MarkerSeverity back to LSP DiagnosticSeverity for code action context. */
function monacoSeverityToLsp(severity: number): number {
  // Monaco: Hint=1, Info=2, Warning=4, Error=8
  // LSP: Error=1, Warning=2, Information=3, Hint=4
  switch (severity) {
    case 8: return 1; // Error
    case 4: return 2; // Warning
    case 2: return 3; // Information
    case 1: return 4; // Hint
    default: return 3;
  }
}

/** Convert an LSP WorkspaceEdit to Monaco's WorkspaceEdit format. */
function lspWorkspaceEditToMonaco(edit: LspWorkspaceEdit): monaco.languages.WorkspaceEdit {
  const edits: monaco.languages.IWorkspaceTextEdit[] = [];

  if (edit.documentChanges && edit.documentChanges.length > 0) {
    for (const docChange of edit.documentChanges) {
      const uri = monaco.Uri.parse(docChange.textDocument.uri);
      for (const textEdit of docChange.edits) {
        edits.push({
          resource: uri,
          textEdit: {
            range: lspToMonacoRange(textEdit.range),
            text: textEdit.newText,
          },
          versionId: undefined,
        });
      }
    }
  } else if (edit.changes) {
    for (const [uriStr, textEdits] of Object.entries(edit.changes)) {
      const uri = monaco.Uri.parse(uriStr);
      for (const textEdit of textEdits) {
        edits.push({
          resource: uri,
          textEdit: {
            range: lspToMonacoRange(textEdit.range),
            text: textEdit.newText,
          },
          versionId: undefined,
        });
      }
    }
  }

  return { edits };
}
