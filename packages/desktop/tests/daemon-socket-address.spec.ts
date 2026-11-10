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

it("keeps long UTF-8 control paths distinct within the Unix socket limit", () => {
  const directory = path.join(os.tmpdir(), "长家-".repeat(40));
  const addresses = ["profile-a", "profile-b"].map(profile => socketAddress(path.join(directory, profile, "daemon-v2.sock")));
  expect(new Set(addresses.map(address => Buffer.from(address).subarray(0, 103).toString())).size).toBe(2);
  if (process.platform !== "win32") expect(addresses.every(address => Buffer.byteLength(address) <= 103)).toBe(true);
});
