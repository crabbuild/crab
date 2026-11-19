import { build } from "esbuild";
import { readFile, writeFile, readdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../../../", import.meta.url));
const output = join(root, "crates/crab-workflow/assets/results-runtime.js");
const result = await build({
  absWorkingDir: root,
  entryPoints: [join(root, "crates/crab-workflow/assets/results-runtime.mjs")],
  nodePaths: [join(root, "packages/ui/node_modules")],
  bundle: true,
  minify: true,
  format: "iife",
  platform: "browser",
  target: "es2022",
  legalComments: "inline",
  write: false,
  metafile: true,
});
const packages = new Map();
for (const input of Object.keys(result.metafile.inputs)) {
  let directory = dirname(resolve(root, input));
  while (directory.startsWith(root)) {
    const entries = await readdir(directory);
    if (entries.includes("package.json")) {
      const pkg = JSON.parse(
        await readFile(join(directory, "package.json"), "utf8"),
      );
      if (!pkg.name || !input.includes("node_modules/")) break;
      const name = entries.find((name) =>
        /^licen[cs]e(?:\.txt|\.md)?$/i.test(name),
      );
      const license = name
        ? await readFile(join(directory, name), "utf8")
        : pkg.name.startsWith("vega")
          ? await readFile(
              join(root, "crates/crab-workflow/assets/VEGA-LICENSE.txt"),
              "utf8",
            )
          : null;
      if (!license)
        throw new Error(`Missing bundled dependency notice: ${pkg.name}`);
      packages.set(`${pkg.name}@${pkg.version}`, license);
      break;
    }
    directory = dirname(directory);
  }
}
const notices = [...packages]
  .sort(([a], [b]) => a.localeCompare(b))
  .map(([name, license]) => `${name}\n${license}`)
  .join("\n\n");
const files = [
  [output, result.outputFiles[0].contents],
  [
    join(root, "crates/crab-workflow/assets/RESULTS-LICENSES.txt"),
    Buffer.from(notices),
  ],
];
for (const [path, bytes] of files) {
  if (process.argv.includes("--check")) {
    if (!(await readFile(path)).equals(Buffer.from(bytes)))
      throw new Error("Regenerate bundled Results chart runtime and notices");
  } else await writeFile(path, bytes);
}
