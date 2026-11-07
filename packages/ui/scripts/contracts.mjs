import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

if (!process.env.CARGO_TARGET_DIR) {
  throw new Error(
    "Set CARGO_TARGET_DIR to this checkout's directory under $HOME/Workspace/crabbuild-target.",
  );
}
const mode = process.argv[2];
if (mode !== "generate" && mode !== "check") {
  throw new Error("Choose generate or check.");
}
const test =
  mode === "generate"
    ? "export_browser_contracts"
    : "browser_contracts_match_server";
const args = [
  "test",
  "-p",
  "crab-server",
  "--locked",
  "--lib",
  `api::contracts::${test}`,
  "--",
];
if (mode === "generate") args.push("--ignored");
const result = spawnSync("cargo", args, {
  cwd: fileURLToPath(new URL("../../../", import.meta.url)),
  stdio: "inherit",
});
if (result.error) throw result.error;
process.exit(result.status ?? 1);
