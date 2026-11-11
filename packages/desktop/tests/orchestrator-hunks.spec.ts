import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { editHunk } from "../electron/orchestrator-hunks";

describe("hunk preimages", () => {
  let directory: string;
  let filePath: string;
  beforeEach(async () => {
    directory = await mkdtemp(path.join(os.tmpdir(), "crab-hunk-"));
    filePath = path.join(directory, "source.txt");
  });
  afterEach(async () => { await rm(directory, { recursive: true, force: true }); });

  it.each([
    { name: "changed file creation", current: "new user contents", oldString: "", newString: "original agent contents", operation: "revert" as const },
    { name: "creation over an existing file", current: "new user contents", oldString: "", newString: "original agent contents", operation: "apply" as const },
    { name: "ambiguous edit", current: "one one", oldString: "one", newString: "two", operation: "apply" as const },
    { name: "ambiguous inverse", current: "two two", oldString: "one", newString: "two", operation: "revert" as const },
    { name: "inverse with missing position", current: "remaining text", oldString: "deleted text", newString: "", operation: "revert" as const },
  ])("rejects $name without changing bytes", async ({ current, oldString, newString, operation }) => {
    await writeFile(filePath, current);
    await expect(editHunk(directory, { filePath, oldString, newString }, operation)).rejects.toThrow();
    expect(await readFile(filePath, "utf8")).toBe(current);
  });

  it("round trips an exact edit containing literal replacement tokens", async () => {
    await writeFile(filePath, "before old after");
    const patch = { filePath, oldString: "old", newString: "$& $1 $$" };
    await editHunk(directory, patch, "apply");
    await editHunk(directory, patch, "revert");
    expect(await readFile(filePath, "utf8")).toBe("before old after");
  });

  it("restores a whole-file deletion represented by an empty file", async () => {
    await writeFile(filePath, "");
    await editHunk(directory, { filePath, oldString: "original bytes", newString: "" }, "revert");
    expect(await readFile(filePath, "utf8")).toBe("original bytes");
  });

  it.each(["relative", "absolute", "parent-link", "leaf-link"])("rejects a %s escape without changing the outside file", async kind => {
    const workspace = path.join(directory, "workspace");
    const outside = path.join(directory, "outside");
    await mkdir(workspace);
    await mkdir(outside);
    const external = path.join(outside, "source.txt");
    await writeFile(external, "original bytes");
    let filePath = "../outside/source.txt";
    if (kind === "absolute") filePath = workspace + "/../outside/source.txt";
    if (kind === "parent-link") { await symlink(outside, path.join(workspace, "link"), "dir"); filePath = "link/source.txt"; }
    if (kind === "leaf-link") { await symlink(external, path.join(workspace, "link.txt")); filePath = "link.txt"; }
    await expect(editHunk(workspace, { filePath, oldString: "original", newString: "changed" }, "apply")).rejects.toThrow();
    expect(await readFile(external, "utf8")).toBe("original bytes");
  });

  it("creates a nested file through the workspace's physical path", async () => {
    await editHunk(directory, { filePath: "new/directory/source.txt", oldString: "", newString: "new bytes" }, "apply");
    expect(await readFile(path.join(directory, "new/directory/source.txt"), "utf8")).toBe("new bytes");
  });
});
