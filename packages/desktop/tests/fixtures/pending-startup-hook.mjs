import { writeFileSync } from "node:fs";

writeFileSync(process.argv[2], String(process.pid));
process.stdin.resume();
process.on("SIGTERM", () => process.exit(0));
process.on("SIGINT", () => process.exit(0));
setTimeout(() => {}, 45_000);
