import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import ts from "typescript";
import { expect, it } from "vitest";

function sourceFiles(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? sourceFiles(path) : path.endsWith(".tsx") ? [path] : [];
  });
}

it("gives slotted controls exactly one child, including explicit JSX whitespace", () => {
  // Radix validates this at runtime; JSX's children type also permits arrays
  // and strings, so typechecking alone cannot catch a broken trigger.
  const invalid: string[] = [];
  for (const file of sourceFiles("src")) {
    const source = ts.createSourceFile(file, readFileSync(file, "utf8"), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
    function visit(node: ts.Node) {
      if (ts.isJsxElement(node) && node.openingElement.attributes.properties.some(attribute => ts.isJsxAttribute(attribute) && attribute.name.getText(source) === "asChild")) {
        const children = node.children.filter(child => !ts.isJsxText(child) || child.text.trim());
        if (children.length !== 1 || ts.isJsxText(children[0]) || ts.isJsxFragment(children[0])) invalid.push(`${file}:${source.getLineAndCharacterOfPosition(node.getStart(source)).line + 1}`);
      }
      ts.forEachChild(node, visit);
    }
    visit(source);
  }
  expect(invalid).toEqual([]);
});
