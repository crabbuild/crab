import { expect, it } from "vitest";
import { FileTree } from "@pierre/trees";
import { compareFileItems } from "./entry-sort";

it("sorts directories before files and names naturally in ascending order", () => {
  const entries = [
    ["zeta.txt", false],
    ["file10.txt", false],
    ["Beta", true],
    ["alpha", true],
    ["file2.txt", false],
  ] as [string, boolean][];
  entries.sort((left, right) =>
    compareFileItems(left[0], left[1], right[0], right[1]),
  );
  expect(entries.map(([name]) => name)).toEqual([
    "alpha",
    "Beta",
    "file2.txt",
    "file10.txt",
    "zeta.txt",
  ]);
});

it("preserves expansion and selection when a remote directory page arrives", () => {
  const selected: string[][] = [];
  const model = new FileTree({
    paths: ["src/", "README.md"],
    initialExpansion: "closed",
    onSelectionChange: (paths) => selected.push([...paths]),
  });
  try {
    const directory = model.getItem("src");
    if (!directory || !("expand" in directory))
      throw new Error("Missing directory");
    directory.expand();
    model.batch([{ type: "add", path: "src/main.rs" }]);
    model.getItem("src/main.rs")?.select();
    model.batch([{ type: "add", path: "src/lib.rs" }]);
    expect(
      model.getVisibleRows(0, model.getVisibleCount()).map((row) => row.path),
    ).toEqual(["src/", "src/lib.rs", "src/main.rs", "README.md"]);
    expect(model.getSelectedPaths()).toEqual(["src/main.rs"]);
    expect(selected).toEqual([["src/main.rs"]]);
  } finally {
    model.cleanUp();
  }
});
