import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useMemo,
  useRef,
} from "react";
import {
  SQLite,
  PostgreSQL,
  sql,
  type SQLNamespace,
} from "@codemirror/lang-sql";
import {
  autocompletion,
  closeBrackets,
  closeBracketsKeymap,
} from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import {
  bracketMatching,
  HighlightStyle,
  syntaxHighlighting,
} from "@codemirror/language";
import { Compartment } from "@codemirror/state";
import {
  drawSelection,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
} from "@codemirror/view";
import { tags } from "@lezer/highlight";
import type { QueryFormat, QueryRelation } from "./data-query";

export type SqlEditorHandle = {
  insertIdentifier: (name: string) => void;
};

type Props = {
  defaultRelation?: string;
  describedBy: string;
  format: QueryFormat;
  onChange: (value: string) => void;
  onRun: (explain: boolean) => void;
  relations: QueryRelation[];
  value: string;
};

function completionSchema(relations: QueryRelation[]): SQLNamespace {
  return Object.fromEntries(
    relations.map((relation) => [
      relation.name,
      relation.fields.map((field) => ({
        label: field.name,
        type: "property",
        detail: field.type.toLowerCase(),
      })),
    ]),
  );
}

function language(
  format: QueryFormat,
  relations: QueryRelation[],
  defaultRelation?: string,
) {
  return sql({
    dialect: format === "sqlite" ? SQLite : PostgreSQL,
    schema: completionSchema(relations),
    defaultTable: defaultRelation ?? relations[0]?.name,
    upperCaseKeywords: true,
  });
}

const editorTheme = EditorView.theme({
  "&": {
    backgroundColor: "var(--bgColor-default)",
    color: "var(--fgColor-default)",
    fontSize: "13px",
  },
  ".cm-content": {
    caretColor: "var(--fgColor-accent)",
    fontFamily: "var(--fontStack-monospace)",
    lineHeight: "20px",
    minHeight: "116px",
    padding: "10px 0",
  },
  ".cm-cursor, .cm-dropCursor": {
    borderLeftColor: "var(--fgColor-accent)",
  },
  ".cm-gutters": {
    backgroundColor: "var(--bgColor-muted)",
    border: "0",
    color: "var(--fgColor-muted)",
  },
  ".cm-activeLine, .cm-activeLineGutter": {
    backgroundColor: "var(--bgColor-accent-muted)",
  },
  ".cm-selectionBackground, ::selection": {
    backgroundColor: "var(--control-transparent-bgColor-selected) !important",
  },
  ".cm-tooltip": {
    backgroundColor: "var(--overlay-bgColor)",
    borderColor: "var(--borderColor-default)",
    borderRadius: "var(--borderRadius-medium)",
    boxShadow: "var(--shadow-floating-small)",
    color: "var(--fgColor-default)",
    overflow: "hidden",
  },
  ".cm-tooltip-autocomplete > ul > li": {
    padding: "4px 8px",
  },
  ".cm-tooltip-autocomplete > ul > li[aria-selected]": {
    backgroundColor: "var(--bgColor-accent-muted)",
    color: "var(--fgColor-default)",
  },
  ".cm-completionDetail": {
    color: "var(--fgColor-muted)",
    fontStyle: "normal",
  },
});

const editorHighlight = HighlightStyle.define([
  { tag: tags.keyword, color: "var(--fgColor-accent)", fontWeight: "600" },
  {
    tag: [tags.string, tags.special(tags.string)],
    color:
      "color-mix(in srgb, var(--fgColor-success) 82%, var(--fgColor-default))",
  },
  { tag: [tags.number, tags.bool, tags.null], color: "var(--fgColor-done)" },
  {
    tag: [tags.lineComment, tags.blockComment],
    color: "var(--fgColor-muted)",
    fontStyle: "italic",
  },
  { tag: [tags.operator, tags.punctuation], color: "var(--fgColor-muted)" },
  { tag: tags.typeName, color: "var(--fgColor-attention)" },
]);

export const SqlEditor = forwardRef<SqlEditorHandle, Props>(function SqlEditor(
  { defaultRelation, describedBy, format, onChange, onRun, relations, value },
  forwardedRef,
) {
  const container = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | undefined>(undefined);
  const languageConfig = useMemo(() => new Compartment(), []);
  const onChangeRef = useRef(onChange);
  const onRunRef = useRef(onRun);
  onChangeRef.current = onChange;
  onRunRef.current = onRun;

  useEffect(() => {
    if (!container.current) return;
    const editor = new EditorView({
      doc: value,
      parent: container.current,
      extensions: [
        lineNumbers(),
        highlightActiveLineGutter(),
        highlightSpecialChars(),
        history(),
        drawSelection(),
        bracketMatching(),
        closeBrackets(),
        autocompletion(),
        highlightActiveLine(),
        keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...historyKeymap]),
        editorTheme,
        syntaxHighlighting(editorHighlight),
        languageConfig.of(language(format, relations, defaultRelation)),
        EditorView.lineWrapping,
        EditorView.contentAttributes.of({
          "aria-describedby": describedBy,
          "aria-label": "SQL query",
          "aria-multiline": "true",
          autocapitalize: "off",
          autocorrect: "off",
          spellcheck: "false",
        }),
        EditorView.domEventHandlers({
          keydown(event) {
            if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
              event.preventDefault();
              onRunRef.current(event.shiftKey);
              return true;
            }
            return false;
          },
        }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged)
            onChangeRef.current(update.state.doc.toString());
        }),
      ],
    });
    view.current = editor;
    return () => {
      view.current = undefined;
      editor.destroy();
    };
  }, [describedBy, languageConfig]);

  useEffect(() => {
    view.current?.dispatch({
      effects: languageConfig.reconfigure(
        language(format, relations, defaultRelation),
      ),
    });
  }, [defaultRelation, format, languageConfig, relations]);

  useEffect(() => {
    const editor = view.current;
    if (!editor || editor.state.doc.toString() === value) return;
    const head = Math.min(editor.state.selection.main.head, value.length);
    editor.dispatch({
      changes: { from: 0, to: editor.state.doc.length, insert: value },
      selection: { anchor: head },
    });
  }, [value]);

  useImperativeHandle(
    forwardedRef,
    () => ({
      insertIdentifier(name) {
        const editor = view.current;
        if (!editor) return;
        const identifier = `"${name.replaceAll('"', '""')}"`;
        const selection = editor.state.selection.main;
        editor.dispatch({
          changes: {
            from: selection.from,
            to: selection.to,
            insert: identifier,
          },
          selection: { anchor: selection.from + identifier.length },
          scrollIntoView: true,
        });
        editor.focus();
      },
    }),
    [],
  );

  return <div className="sql-editor" ref={container} />;
});
