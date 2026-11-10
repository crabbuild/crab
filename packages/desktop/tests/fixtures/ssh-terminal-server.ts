import { Server, Client, utils, type Connection, type ServerChannel, type PseudoTtyInfo } from "ssh2";
import { generateKeyPairSync, createHash } from "node:crypto";
import { spawn, type ChildProcess } from "node:child_process";
import { spawn as spawnPty, type IPty } from "node-pty";

/** Loopback SSH server with real command processes and PTYs; no mocked terminal I/O. */
export async function terminalSshServer(root: string, remotePath = process.env.PATH ?? "") {
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048, privateKeyEncoding: { type: "pkcs1", format: "pem" }, publicKeyEncoding: { type: "spki", format: "pem" } });
  const parsed = utils.parseKey(privateKey);
  if (parsed instanceof Error || Array.isArray(parsed)) throw new Error("Cannot create fixture SSH host key");
  const fingerprint = createHash("sha256").update(parsed.getPublicSSH()).digest("hex");
  const connections = new Set<Connection>();
  const ptys = new Set<IPty>();
  const children = new Set<ChildProcess>();
  const env = { ...process.env, PATH: remotePath, HOME: root, SHELL: "/bin/sh", TMUX_TMPDIR: root, TERM: "xterm-256color", LANG: "en_US.UTF-8" };
  // macOS strips DYLD_* when launching its signed /bin/sh. Restore the
  // test-only library path inside that shell for uninstalled fixture bottles.
  const libraryPath = process.env.CRAB_TEST_DYLD_LIBRARY_PATH ?? process.env.DYLD_LIBRARY_PATH;
  const prefix = libraryPath ? `export DYLD_LIBRARY_PATH='${libraryPath.replace(/'/g, "'\\''")}'; ` : "";
  const server = new Server({ hostKeys: [privateKey] }, connection => {
    connections.add(connection);
    connection.on("error", () => {});
    connection.on("close", () => connections.delete(connection));
    connection.on("authentication", context => { if (context.method === "password" && context.password === "fixture-only") context.accept(); else context.reject(); });
    connection.on("ready", () => connection.on("session", accept => {
      const session = accept();
      let dimensions = { cols: 80, rows: 24 };
      let hasPty = false;
      let terminal: IPty | undefined;
      session.on("pty", (accept, _reject, info: PseudoTtyInfo) => { dimensions = info; hasPty = true; accept?.(); });
      session.on("window-change", (accept, _reject, info: PseudoTtyInfo) => { terminal?.resize(info.cols, info.rows); accept?.(); });
      session.on("exec", (accept, _reject, info) => {
        const stream = accept() as ServerChannel;
        if (hasPty) {
          terminal = spawnPty("/bin/sh", ["-c", prefix + info.command], { ...dimensions, cwd: root, env, name: "xterm-256color" });
          ptys.add(terminal);
          const active = terminal;
          active.onData(data => stream.write(data));
          active.onExit(({ exitCode }) => { ptys.delete(active); stream.exit(exitCode); stream.end(); });
          stream.on("data", (data: Buffer) => active.write(data.toString("utf8")));
          stream.on("close", () => active.kill());
        } else {
          const child = spawn("/bin/sh", ["-c", prefix + info.command], { cwd: root, env });
          children.add(child);
          stream.pipe(child.stdin);
          child.stdout.pipe(stream, { end: false });
          child.stderr.pipe(stream.stderr, { end: false });
          child.on("close", code => { children.delete(child); stream.exit(code ?? 1); stream.end(); });
          stream.on("close", () => child.kill());
        }
      });
    }));
  });
  await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Fixture SSH server has no port");
  return {
    env,
    connection: { port: address.port, fingerprint },
    async connect() {
      const client = new Client();
      await new Promise<void>((resolve, reject) => {
        client.once("error", reject).once("ready", resolve).connect({
          host: "127.0.0.1", port: address.port, username: "fixture", password: "fixture-only",
          hostHash: "sha256", hostVerifier: (hash: string) => hash === fingerprint,
        });
      });
      return client;
    },
    async close() {
      for (const connection of connections) connection.end();
      for (const pty of ptys) pty.kill();
      for (const child of children) child.kill();
      await new Promise<void>(resolve => server.close(() => resolve()));
    },
  };
}
