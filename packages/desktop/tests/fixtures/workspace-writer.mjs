import { spawn } from "node:child_process";
import { writeFileSync, appendFileSync } from "node:fs";
import path from "node:path";

const [directory, role = "parent"] = process.argv.slice(2);
writeFileSync(path.join(directory, `${role}.pid`), String(process.pid));
process.on("SIGTERM", () => writeFileSync(path.join(directory, `${role}.term`), "received"));
setInterval(() => appendFileSync(path.join(directory, `${role}.writes`), "x"), 25);
if (role === "parent") spawn(process.execPath, [import.meta.filename, directory, "child"], { stdio: "ignore", env: process.env });
