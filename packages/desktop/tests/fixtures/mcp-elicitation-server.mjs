import { createInterface } from "node:readline";
import { appendFileSync, writeFileSync } from "node:fs";

const proofPath = process.argv[2];
const duringTool = process.argv[3] === "tool";
const urlMode = process.argv[4] === "url";
const tracePath = `${proofPath}.trace`;
const send = frame => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", ...frame }) + "\n");
let pending;
let elicited = false;
const elicit = frame => {
  pending = frame;
  const params = urlMode ? {
    mode: "url", message: "Fixture: confirm external consent", url: "https://consent.example.invalid/fixture", elicitationId: "fixture-consent",
  } : { mode: "form", message: "Fixture: confirm typed values", requestedSchema: {
    type: "object", properties: { count: { type: "integer", minimum: 0 }, enabled: { type: "boolean", default: false } }, required: ["count", "enabled"],
  } };
  send({ id: "fixture-form", method: "elicitation/create", params });
};
appendFileSync(tracePath, "started\n");
createInterface({ input: process.stdin }).on("line", line => {
  const frame = JSON.parse(line);
  appendFileSync(tracePath, JSON.stringify({ method: frame.method, id: frame.id, action: frame.result?.action, error: frame.error?.message }) + "\n");
  if (frame.method === "initialize") {
    send({ id: frame.id, result: { protocolVersion: frame.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: "CompatibilityFixture", version: "1.0.0" } } });
  } else if (frame.method === "tools/list") {
    if (duringTool) { send({ id: frame.id, result: { tools: [{ name: "confirm_values", description: "Confirm the compatibility fixture's typed values with the user", inputSchema: { type: "object", properties: {} } }] } }); return; }
    if (elicited) { send({ id: frame.id, result: { tools: [] } }); return; }
    elicited = true;
    elicit(frame);
  } else if (frame.method === "tools/call") {
    elicit(frame);
  } else if (frame.id === "fixture-form") {
    writeFileSync(proofPath, JSON.stringify(frame.result ?? frame.error));
    send({ id: pending.id, result: pending.method === "tools/list" ? { tools: [] } : { content: [{ type: "text", text: JSON.stringify(frame.result ?? frame.error) }] } });
  } else if (frame.method && frame.id !== undefined) {
    if (frame.method === "ping") send({ id: frame.id, result: {} });
    else send({ id: frame.id, error: { code: -32601, message: "Unsupported fixture method" } });
  }
});
