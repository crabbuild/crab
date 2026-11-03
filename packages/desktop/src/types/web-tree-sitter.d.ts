// Type declarations for web-tree-sitter.
// These will be superseded by the package's own types once `npm install` runs.

declare module "web-tree-sitter" {
  export interface Point {
    row: number;
    column: number;
  }

  export interface SyntaxNode {
    id: number;
    type: string;
    text: string;
    startPosition: Point;
    endPosition: Point;
    startIndex: number;
    endIndex: number;
    parent: SyntaxNode | null;
    children: SyntaxNode[];
    namedChildren: SyntaxNode[];
    childCount: number;
    namedChildCount: number;
    firstChild: SyntaxNode | null;
    lastChild: SyntaxNode | null;
    nextSibling: SyntaxNode | null;
    previousSibling: SyntaxNode | null;
    nextNamedSibling: SyntaxNode | null;
    previousNamedSibling: SyntaxNode | null;
    descendantsOfType(type: string): SyntaxNode[];
    childForFieldName(fieldName: string): SyntaxNode | null;
    walk(): TreeCursor;
  }

  export interface TreeCursor {
    nodeType: string;
    nodeText: string;
    startPosition: Point;
    endPosition: Point;
    currentNode: SyntaxNode;
    gotoFirstChild(): boolean;
    gotoNextSibling(): boolean;
    gotoParent(): boolean;
  }

  export interface Tree {
    rootNode: SyntaxNode;
    delete(): void;
  }

  export interface Language {
    // Language instance methods
  }

  namespace Language {
    function load(path: string): Promise<Language>;
  }

  interface InitOptions {
    locateFile?: (scriptName: string, scriptDirectory: string) => string;
  }

  class Parser {
    static init(options?: InitOptions): Promise<void>;
    static Language: typeof Language;
    constructor();
    setLanguage(language: Language): void;
    parse(input: string, oldTree?: Tree): Tree;
    delete(): void;
  }

  namespace Parser {
    export type Language = import("web-tree-sitter").Language;
    export type Tree = import("web-tree-sitter").Tree;
  }

  export default Parser;
}
