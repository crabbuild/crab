import { spawn } from "node:child_process";
import { writeFileSync, appendFileSync } from "node:fs";
import path from "node:path";

const [directory, role = "parent"] = process.argv.slice(2);
writeFileSync(path.join(directory, `${role}.pid`), String(process.pid));
process.on("SIGTERM", () => writeFileSync(path.join(directory, `${role}.term`), "received"));
setInterval(() => appendFileSync(path.join(directory, `${role}.writes`), "x"), 25);
if (["parent", "ignored", "inherited"].includes(role)) {
  const child = spawn(process.execPath, [import.meta.filename, directory, "child"], { stdio: role === "inherited" ? "inherit" : "ignore", env: process.env });
  if (child.pid) writeFileSync(path.join(directory, "child.pid"), String(child.pid));
  if (role !== "parent") setTimeout(() => process.exit(0), 200);
}
