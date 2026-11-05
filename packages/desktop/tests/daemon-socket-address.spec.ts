import { it, expect } from "vitest";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { socketAddress } from "../electron/daemon/socket-address";

it("connects through the platform IPC transport for a control-file path", async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "crab-ipc-"));
  const address = socketAddress(path.join(root, "daemon-v2.sock"));
  const server = net.createServer(socket => socket.end("daemon-ready"));
  let client: net.Socket | undefined;
  try {
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(address, resolve);
    });
    const result = await new Promise<string>((resolve, reject) => {
      client = net.createConnection(address);
      let data = "";
      client.setEncoding("utf8");
      client.on("data", chunk => { data += chunk; });
      client.once("end", () => resolve(data));
      client.once("error", reject);
    });
    expect(result).toBe("daemon-ready");
  } finally {
    client?.destroy();
    if (server.listening) await new Promise<void>(resolve => server.close(() => resolve()));
    fs.rmSync(root, { recursive: true, force: true });
  }
});

it("isolates profiles and protocol versions in the IPC namespace", () => {
  const files = [
    path.join(os.tmpdir(), "profile-a", "daemon-v1.sock"),
    path.join(os.tmpdir(), "profile-a", "daemon-v2.sock"),
    path.join(os.tmpdir(), "profile-b", "daemon-v2.sock"),
  ];
  expect(new Set(files.map(socketAddress)).size).toBe(files.length);
});
