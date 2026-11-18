import type { Node, Query, Tree } from "web-tree-sitter";
import type { CodeSymbol, CodeSymbolLanguage, CodeSymbolRole } from ".";

function containingName(node: Node, language: CodeSymbolLanguage) {
  let parent = node.parent;
  while (parent) {
    if (language === "rust") {
      if (parent.type === "impl_item") {
        const type = parent.childForFieldName("type")?.text;
        const trait = parent.childForFieldName("trait")?.text;
        if (type) return trait ? `impl ${trait} for ${type}` : `impl ${type}`;
      }
      if (parent.type === "trait_item") {
        const name = parent.childForFieldName("name")?.text;
        if (name) return `trait ${name}`;
      }
      if (parent.type === "mod_item") {
        const name = parent.childForFieldName("name")?.text;
        if (name) return `mod ${name}`;
      }
    } else if (
      [
        "class_declaration",
        "class",
        "class_definition",
        "class_specifier",
        "interface_declaration",
        "struct_specifier",
      ].includes(parent.type)
    ) {
      const name = parent.childForFieldName("name")?.text;
      if (name) return name;
    }
    parent = parent.parent;
  }
  return undefined;
}

function kindPriority(kind: string) {
  if (kind === "method") return 2;
  if (kind === "function") return 1;
  return 0;
}

export function extractSymbols(
  language: CodeSymbolLanguage,
  tree: Tree | null | undefined,
  query: Query,
) {
  if (!tree) return [];
  const symbols = new Map<string, CodeSymbol>();
  const record = (node: Node, role: CodeSymbolRole, kind: string) => {
    const key = `${node.startIndex}:${node.endIndex}`;
    const current = symbols.get(key);
    if (
      current &&
      ((current.role === "definition" && role === "reference") ||
        (current.role === role &&
          kindPriority(current.kind) >= kindPriority(kind)))
    )
      return;
    symbols.set(key, {
      id: `${role}:${key}`,
      name: node.text,
      role,
      kind,
      line: node.startPosition.row + 1,
      // Web Tree-sitter parses JS strings as UTF-16, matching Shiki token offsets.
      // Converting these columns from UTF-8 shifts matches after non-ASCII text.
      column: node.startPosition.column,
      endColumn: node.endPosition.column,
      container: containingName(node, language),
    });
  };

  for (const match of query.matches(tree.rootNode)) {
    const names = match.captures.filter((capture) => capture.name === "name");
    for (const capture of match.captures) {
      const [role, kind] = capture.name.split(".");
      if ((role !== "definition" && role !== "reference") || !kind) continue;
      // Tag queries associate @name with the match; a call capture can be on
      // its arguments rather than an ancestor of the method's name.
      for (const { node } of names) record(node, role, kind);
    }
  }

  // Tags omit value uses such as passing a function as a callback. Identifier
  // leaves include those uses without matching names in comments or strings.
  const identifierTypes = tree.language.types.filter((type) =>
    /identifier(?:_pattern)?$/.test(type),
  );
  for (const node of tree.rootNode.descendantsOfType(identifierTypes)) {
    if (node.namedChildCount === 0) record(node, "reference", "identifier");
  }

  return [...symbols.values()].sort(
    (left, right) => left.line - right.line || left.column - right.column,
  );
}
