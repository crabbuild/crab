import { generateKeyPairSync, timingSafeEqual } from "node:crypto";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { once } from "node:events";
import { mkdirSync, realpathSync, writeFileSync, type Stats } from "node:fs";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { Server, utils, type Connection } from "ssh2";

export async function sshFixture(base: string, environment: NodeJS.ProcessEnv) {
  const homePath = path.join(base, "remote 'home'"); mkdirSync(homePath);
  const home = realpathSync(homePath);
  const keys = () => generateKeyPairSync("rsa", { modulusLength: 2048, privateKeyEncoding: { type: "pkcs1", format: "pem" }, publicKeyEncoding: { type: "pkcs1", format: "pem" } });
  const host = keys(), auth = keys();
  const hostKey = utils.parseKey(host.privateKey), authKey = utils.parseKey(auth.privateKey);
  if (hostKey instanceof Error) throw hostKey;
  if (authKey instanceof Error) throw authKey;
  const keyPath = path.join(base, "client-key"); writeFileSync(keyPath, auth.privateKey, { mode: 0o600 });
  const connections = new Set<Connection>();
  const commands = new Set<ChildProcessWithoutNullStreams>();
  const failures: Error[] = [];
  let uploadedBytes = 0, openFiles = 0, connectionAttempts = 0, accepting = true;
  const requests: Array<{ method: string; repo_id?: string; path?: string; storage_connection_id?: string }> = [];
  const inside = (name: string) => {
    const resolved = name === "." ? home : path.resolve(name);
    if (resolved !== home && !resolved.startsWith(home + path.sep)) throw Object.assign(new Error("Outside SSH fixture"), { code: "EACCES" });
    return resolved;
  };
  const attrs = (stat: Stats) => ({ mode: stat.mode, uid: stat.uid, gid: stat.gid, size: stat.size, atime: Math.floor(stat.atimeMs / 1000), mtime: Math.floor(stat.mtimeMs / 1000) });
  const server = new Server({ hostKeys: [host.privateKey] }, connection => {
    connections.add(connection); connection.on("close", () => connections.delete(connection)); connection.on("error", () => {});
    connectionAttempts++;
    if (!accepting) { connection.end(); return; }
    connection.on("authentication", context => {
      if (context.method !== "publickey" || !context.key.data.equals(authKey.getPublicSSH())) { context.reject(); return; }
      if (context.signature && (!context.blob || authKey.verify(context.blob, context.signature, context.hashAlgo) !== true)) { context.reject(); return; }
      context.accept();
    });
    connection.on("ready", () => connection.on("session", accept => {
      const session = accept();
      session.on("exec", (accept, reject, request) => {
        if (request.command !== "uname -s && uname -m" && request.command !== "echo ok" && !request.command.includes(path.dirname(home))) { reject(); return; }
        const stream = accept();
        const child = spawn("/bin/sh", ["-c", request.command], { cwd: home, env: environment, stdio: "pipe" });
        commands.add(child);
        child.on("error", error => failures.push(error));
        let incoming = Buffer.alloc(0);
        stream.on("data", (bytes: Buffer) => {
          incoming = Buffer.concat([incoming, bytes]);
          let newline: number;
          while ((newline = incoming.indexOf(10)) >= 0) {
            const line = incoming.subarray(0, newline); incoming = incoming.subarray(newline + 1);
            const value = JSON.parse(line.toString("utf8")) as { method: string; params?: { repo_id?: string; path?: string; __storage_auth?: { id?: string } } };
            const identity = value.params?.__storage_auth?.id;
            requests.push({ method: value.method, repo_id: value.params?.repo_id, path: value.params?.path, ...(identity ? { storage_connection_id: identity } : {}) });
          }
        });
        stream.pipe(child.stdin);
        child.stdin.on("error", () => {});
        child.stdout.pipe(stream, { end: false }); child.stderr.pipe(stream.stderr, { end: false });
        stream.on("close", () => child.stdin.end());
        child.once("close", code => { commands.delete(child); stream.exit(code ?? 1); stream.end(); });
      });
      session.on("sftp", accept => {
        const sftp = accept(), status = utils.sftp.STATUS_CODE;
        const files = new Map<number, fs.FileHandle>();
        const directories = new Map<number, { names: string[]; directory: string; offset: number }>();
        let next = 1;
        const handle = (id: number) => { const value = Buffer.alloc(4); value.writeUInt32BE(id); return value; };
        const id = (value: Buffer) => value.length === 4 ? value.readUInt32BE() : -1;
        const fail = (request: number, error: unknown) => {
          const code = (error as NodeJS.ErrnoException).code;
          sftp.status(request, code === "ENOENT" ? status.NO_SUCH_FILE : code === "EACCES" ? status.PERMISSION_DENIED : status.FAILURE);
        };
        const operation = (request: number, work: () => Promise<void>) => { void work().catch(error => fail(request, error)); };
        sftp.on("REALPATH", (request, name) => operation(request, async () => {
          const filename = await fs.realpath(inside(name)); inside(filename);
          sftp.name(request, [{ filename, longname: filename, attrs: attrs(await fs.stat(filename)) }]);
        }));
        sftp.on("STAT", (request, name) => operation(request, async () => sftp.attrs(request, attrs(await fs.stat(inside(name))))));
        sftp.on("LSTAT", (request, name) => operation(request, async () => sftp.attrs(request, attrs(await fs.lstat(inside(name))))));
        sftp.on("OPEN", (request, name, flags) => operation(request, async () => {
          const mode = utils.sftp.flagsToString(flags);
          if (!mode) throw new Error("Unsupported SFTP flags");
          const file = await fs.open(inside(name), mode, 0o600), value = next++;
          files.set(value, file); openFiles++;
          sftp.handle(request, handle(value));
        }));
        sftp.on("FSTAT", (request, value) => operation(request, async () => {
          const file = files.get(id(value)); if (!file) throw new Error("Missing SFTP handle");
          sftp.attrs(request, attrs(await file.stat()));
        }));
        sftp.on("WRITE", (request, value, offset, bytes) => operation(request, async () => {
          const file = files.get(id(value)); if (!file) throw new Error("Missing SFTP handle");
          const result = await file.write(bytes, 0, bytes.length, offset);
          if (result.bytesWritten !== bytes.length) throw new Error("Incomplete fixture write");
          uploadedBytes += result.bytesWritten; sftp.status(request, status.OK);
        }));
        sftp.on("READ", (request, value, offset, length) => operation(request, async () => {
          const file = files.get(id(value)); if (!file) throw new Error("Missing SFTP handle");
          const data = Buffer.alloc(length), result = await file.read(data, 0, length, offset);
          if (result.bytesRead) sftp.data(request, data.subarray(0, result.bytesRead)); else sftp.status(request, status.EOF);
        }));
        sftp.on("CLOSE", (request, value) => operation(request, async () => {
          const number = id(value), file = files.get(number);
          if (file) { files.delete(number); await file.close(); openFiles--; }
          else if (!directories.delete(number)) throw new Error("Missing SFTP handle");
          sftp.status(request, status.OK);
        }));
        sftp.on("RENAME", (request, from, to) => operation(request, async () => { await fs.rename(inside(from), inside(to)); sftp.status(request, status.OK); }));
        sftp.on("OPENDIR", (request, name) => operation(request, async () => {
          const directory = inside(name), names = await fs.readdir(directory), value = next++;
          directories.set(value, { directory, names, offset: 0 }); sftp.handle(request, handle(value));
        }));
        sftp.on("READDIR", (request, value) => operation(request, async () => {
          const directory = directories.get(id(value)); if (!directory) throw new Error("Missing directory handle");
          const names = directory.names.slice(directory.offset, directory.offset + 64); directory.offset += names.length;
          if (!names.length) { sftp.status(request, status.EOF); return; }
          const entries = await Promise.all(names.map(async filename => ({ filename, longname: filename, attrs: attrs(await fs.lstat(path.join(directory.directory, filename))) })));
          sftp.name(request, entries);
        }));
        sftp.on("end", () => sftp.end());
        sftp.on("close", () => {
          for (const file of files.values()) void file.close().then(() => { openFiles--; }, error => failures.push(error));
          files.clear(); directories.clear();
        });
      });
    }));
  });
  server.on("error", (error: Error) => failures.push(error)); server.listen(0, "127.0.0.1"); await once(server, "listening");
  const address = server.address(); if (!address || typeof address === "string") throw new Error("Missing SSH fixture address");
  return {
    home, keyPath, port: address.port, commands, failures, requests,
    verify: async (_hostname: string, _port: number, key: Buffer) => key.length === hostKey.getPublicSSH().length && timingSafeEqual(key, hostKey.getPublicSSH()),
    get uploadedBytes() { return uploadedBytes; }, get openFiles() { return openFiles; },
    get connectionAttempts() { return connectionAttempts; },
    setAccepting(value: boolean) { accepting = value; },
    async close() {
      connections.forEach(connection => connection.end());
      await Promise.all([...commands].map(child => { const exited = once(child, "close"); child.stdin.end(); return exited; }));
      await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
      if (failures.length) throw new AggregateError(failures, "SSH fixture failures");
    },
  };
}
