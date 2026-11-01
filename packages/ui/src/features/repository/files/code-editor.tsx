import {
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Compartment, EditorState } from "@codemirror/state";
import {
  drawSelection,
  EditorView,
  keymap,
  lineNumbers,
  placeholder,
} from "@codemirror/view";
import {
  defaultKeymap,
  history,
  historyKeymap,
  indentWithTab,
} from "@codemirror/commands";
import { bracketMatching, indentUnit } from "@codemirror/language";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";

const theme = EditorView.theme({
  "&": {
    backgroundColor: "var(--background)",
    color: "var(--foreground)",
    fontSize: "var(--text-caption)",
  },
  // The enclosing editor surface draws one focus boundary around its gutter.
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.6" },
  ".cm-content": { padding: "12px 0", caretColor: "var(--foreground)" },
  ".cm-line": { padding: "0 12px" },
  ".cm-gutters": {
    backgroundColor: "var(--background)",
    color: "var(--muted-foreground)",
    border: "0",
  },
  ".cm-lineNumbers .cm-gutterElement": { minWidth: "40px", padding: "0 8px" },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--foreground)" },
  ".cm-selectionBackground, ::selection": {
    backgroundColor: "var(--secondary) !important",
  },
  ".cm-placeholder": { color: "var(--muted-foreground)" },
});

export function CodeEditor({
  label,
  content,
  setContent,
  preview,
  autoFocus = false,
}: {
  label: string;
  content: string;
  setContent: (content: string) => void;
  preview: ReactNode;
  autoFocus?: boolean;
}) {
  const id = useId();
  const container = useRef<HTMLDivElement>(null);
  const editor = useRef<EditorView | undefined>(undefined);
  const onChange = useRef(setContent);
  onChange.current = setContent;
  const [tab, setTab] = useState("edit");
  const [indent, setIndent] = useState("spaces");
  const [size, setSize] = useState("2");
  const [wrap, setWrap] = useState("off");
  const config = useMemo(() => new Compartment(), []);

  useEffect(() => {
    if (!container.current) return;
    const view = new EditorView({
      parent: container.current,
      doc: content,
      extensions: [
        lineNumbers(),
        history(),
        drawSelection(),
        bracketMatching(),
        theme,
        placeholder("Enter file contents here"),
        keymap.of([indentWithTab, ...defaultKeymap, ...historyKeymap]),
        config.of([]),
        EditorView.contentAttributes.of({
          "aria-label": label,
          "aria-describedby": `${id}-help`,
          "aria-multiline": "true",
          autocapitalize: "off",
          autocorrect: "false",
          spellcheck: "false",
        }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) onChange.current(update.state.doc.toString());
        }),
      ],
    });
    editor.current = view;
    if (autoFocus) view.focus();
    return () => {
      editor.current = undefined;
      view.destroy();
    };
  }, [autoFocus, config, id, label]);

  useEffect(() => {
    editor.current?.dispatch({
      effects: config.reconfigure([
        EditorState.tabSize.of(Number(size)),
        indentUnit.of(indent === "tabs" ? "\t" : " ".repeat(Number(size))),
        ...(wrap === "on" ? [EditorView.lineWrapping] : []),
      ]),
    });
  }, [config, indent, size, wrap]);

  useEffect(() => {
    const view = editor.current;
    if (!view || view.state.doc.toString() === content) return;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: content },
    });
  }, [content]);

  return (
    <Tabs className="file-code-editor" value={tab} onValueChange={setTab}>
      <div className="file-editor-toolbar">
        <TabsList aria-label="File editor mode">
          <TabsTrigger value="edit">Edit</TabsTrigger>
          <TabsTrigger value="preview">Preview</TabsTrigger>
        </TabsList>
        <div
          className="file-editor-settings"
          role="group"
          aria-label="Editor settings"
        >
          <Select value={indent} onValueChange={setIndent}>
            <SelectTrigger size="sm" aria-label="Indentation">
              <SelectValue />
            </SelectTrigger>
            <SelectContent position="popper" align="end">
              <SelectGroup>
                <SelectItem value="spaces">Spaces</SelectItem>
                <SelectItem value="tabs">Tabs</SelectItem>
              </SelectGroup>
            </SelectContent>
          </Select>
          <Select value={size} onValueChange={setSize}>
            <SelectTrigger size="sm" aria-label="Tab size">
              <SelectValue />
            </SelectTrigger>
            <SelectContent position="popper" align="end">
              <SelectGroup>
                {["2", "4", "8"].map((value) => (
                  <SelectItem key={value} value={value}>
                    {value}
                  </SelectItem>
                ))}
              </SelectGroup>
            </SelectContent>
          </Select>
          <Select value={wrap} onValueChange={setWrap}>
            <SelectTrigger size="sm" aria-label="Line wrapping">
              <SelectValue />
            </SelectTrigger>
            <SelectContent position="popper" align="end">
              <SelectGroup>
                <SelectItem value="off">No wrap</SelectItem>
                <SelectItem value="on">Soft wrap</SelectItem>
              </SelectGroup>
            </SelectContent>
          </Select>
        </div>
      </div>
      <TabsContent
        value="edit"
        forceMount
        hidden={tab !== "edit"}
        className="file-code-edit"
      >
        <div ref={container} className="file-code-editor-surface" />
      </TabsContent>
      <TabsContent value="preview" className="file-code-preview">
        {preview}
      </TabsContent>
      <p className="file-editor-help" id={`${id}-help`}>
        Press <kbd>Esc</kbd> then <kbd>Tab</kbd> to move focus out of the
        editor.
      </p>
    </Tabs>
  );
}
