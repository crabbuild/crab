import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";
import { readSshRuntime } from "../electron/ssh-deployer";

let directory: string;
const hash = (bytes: string) => createHash("sha256").update(bytes).digest("hex");
const version = "2.0.0-alpha.1";
const descriptor = () => ({ version, platform: "linux", arch: "arm64", hashes: { crab: hash("CLI"), "crab-desktop-agent": hash("sidecar") }, archive: { sha256: hash("archive"), bytes: 7 } });
const metadata = (value: unknown) => writeFile(path.join(directory, "linux-arm64.json"), JSON.stringify(value));

beforeEach(async () => {
  directory = await mkdtemp(path.join(tmpdir(), "crab-ssh-catalog-"));
  await metadata(descriptor()); await writeFile(path.join(directory, "linux-arm64.tar.gz"), "archive");
});
afterEach(async () => { await rm(directory, { recursive: true, force: true }); });

describe("verified SSH runtime catalog", () => {
  it("binds the selected target and executable pair to the verified archive", async () => {
    const result = await readSshRuntime(directory, "linux-arm64", version);
    expect(result).toMatchObject(descriptor());
    expect(result.file).toBe(path.join(directory, "linux-arm64.tar.gz"));
    expect(result.id).toBe(hash(`${result.hashes.crab}\n${result.hashes["crab-desktop-agent"]}\n`));
  });

  it("keeps executable identity stable when only archive packaging changes", async () => {
    const first = await readSshRuntime(directory, "linux-arm64", version);
    await writeFile(first.file, "repacked");
    await metadata({ ...descriptor(), archive: { sha256: hash("repacked"), bytes: 8 } });
    expect((await readSshRuntime(directory, "linux-arm64", version)).id).toBe(first.id);
  });

  it.each([
    null, {}, { ...descriptor(), version: "1.0.0" }, { ...descriptor(), platform: "darwin" },
    { ...descriptor(), arch: "x64" }, { ...descriptor(), hashes: { crab: hash("CLI") } },
    { ...descriptor(), hashes: { ...descriptor().hashes, extra: hash("unexpected") } },
    { ...descriptor(), archive: { sha256: "bad", bytes: 7 } },
    { ...descriptor(), archive: { sha256: hash("archive"), bytes: 0 } },
  ])("rejects a malformed or mismatched descriptor: %j", async value => {
    await metadata(value);
    await expect(readSshRuntime(directory, "linux-arm64", version)).rejects.toMatchObject({ code: "RUNTIME_INVALID" });
  });

  it("rejects damaged archives without trusting a matching version", async () => {
    await writeFile(path.join(directory, "linux-arm64.tar.gz"), "damaged");
    await expect(readSshRuntime(directory, "linux-arm64", version)).rejects.toMatchObject({ code: "RUNTIME_INVALID" });
    expect(await readFile(path.join(directory, "linux-arm64.tar.gz"), "utf8")).toBe("damaged");
  });

  it("rejects targets outside the supported catalog before reading paths", async () => {
    await expect(readSshRuntime(directory, "../linux-arm64", version)).rejects.toMatchObject({ code: "WRONG_ARCH" });
  });

  it("honors cancellation before reading a runtime", async () => {
    const controller = new AbortController(); controller.abort();
    await expect(readSshRuntime(directory, "linux-arm64", version, controller.signal)).rejects.toHaveProperty("name", "AbortError");
  });
});
