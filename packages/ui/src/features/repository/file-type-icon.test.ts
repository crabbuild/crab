import { describe, expect, it } from "vitest";
import { resolveFileTypeIcon } from "./file-type-icon";

describe("resolveFileTypeIcon", () => {
  it("resolves built-in @pierre/trees tokens and palette colors", () => {
    expect(resolveFileTypeIcon("README.md")).toMatchObject({
      name: "file-tree-builtin-markdown",
      token: "markdown",
      color: "var(--file-icon-green)",
      width: 16,
      height: 16,
      viewBox: "0 0 16 16",
    });

    expect(resolveFileTypeIcon("crates/cellar/src/lib.rs")).toMatchObject({
      name: "file-tree-builtin-rust",
      token: "rust",
      color: "var(--file-icon-orange)",
    });

    expect(resolveFileTypeIcon("src/index.ts")).toMatchObject({
      name: "file-tree-builtin-typescript",
      token: "typescript",
      color: "var(--file-icon-blue)",
    });

    expect(resolveFileTypeIcon("Dockerfile")).toMatchObject({
      name: "file-tree-builtin-docker",
      token: "docker",
      color: "var(--file-icon-blue)",
    });

    expect(resolveFileTypeIcon("package.json")).toMatchObject({
      name: "file-tree-builtin-json",
      token: "json",
      color: "var(--file-icon-orange)",
    });

    expect(resolveFileTypeIcon("Cargo.toml")).toMatchObject({
      name: "file-tree-builtin-default",
      token: "default",
      color: "var(--file-icon-gray)",
    });
  });
});
