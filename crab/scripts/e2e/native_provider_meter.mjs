import http from 'node:http';
import fs from 'node:fs';
import cluster from 'node:cluster';
import { once } from 'node:events';
import { pathToFileURL } from 'node:url';

function operation(method, parsed, headers) {
  if (method === 'HEAD') return 'head';
  const query = parsed.search === '' ? null : parsed.searchParams;
  if (method === 'GET') return query?.has('list-type') || query?.has('versions') ? 'list' : 'get';
  if (method === 'PUT') {
    if ('x-amz-copy-source' in headers) return 'copy';
    return query?.has('partNumber') ? 'multipart_part' : 'put';
  }
  if (method === 'POST') {
    if (query?.has('uploads')) return 'multipart_create';
    if (query?.has('uploadId')) return 'multipart_complete';
    return query?.has('delete') ? 'delete_batch' : 'post';
  }
  if (method === 'DELETE') return query?.has('uploadId') ? 'multipart_abort' : 'delete';
  return method.toLowerCase();
}

function requestKey(parsed, op) {
  const key = decodeURIComponent(parsed.pathname).replace(/^\/+/, '');
  const prefix = op === 'list' ? parsed.searchParams.get('prefix') : null;
  return op === 'list' && prefix ? key.replace(/\/+$/, '') + '/' + prefix.replace(/^\/+/, '') : key;
}

const NESTED_CATEGORIES = new Set(['git_object_catalog_db', 'v2', 'git-visibility', 'locks', 'manifests', 'metadata', 'ref-journal', 'refs']);

function category(key) {
  const separator = key.indexOf('/');
  const first = separator < 0 ? key : key.slice(0, separator);
  if (!NESTED_CATEGORIES.has(first)) return first || 'repository-root';
  const parts = key.split('/');
  if (parts.length > 2 && first === 'metadata' && ['pack', 'shard'].includes(parts[1])) return parts.slice(0, 3).join('/');
  if (parts.length > 2 && first === 'locks' && parts[1] === 'internal') return parts.slice(0, 3).join('/');
  return parts.slice(0, 2).join('/');
}

function increment(counter, name, amount = 1) {
  counter[name] = (counter[name] || 0) + amount;
}

export class Counter {
  constructor(prefix) {
    this.prefix = prefix.replace(/^\/+|\/+$/g, '') + '/';
    this.value = { requests: 0, request_body_bytes: 0, response_body_bytes: 0 };
    for (const field of ['methods', 'operations', 'categories', 'classes', 'class_request_body_bytes', 'class_response_body_bytes', 'statuses', 'proxy_errors', 'latency_ms_histogram']) this.value[field] = Object.create(null);
  }
  record(method, path, headers, requestBytes, responseBytes, status, elapsedMs, error) {
    const parsed = new URL(path, 'http://meter');
    const op = operation(method, parsed, headers);
    const key = requestKey(parsed, op);
    const group = key.startsWith(this.prefix) ? category(key.slice(this.prefix.length)) : 'outside-repository';
    const requestClass = group + ':' + op;
    this.value.requests++;
    this.value.request_body_bytes += requestBytes;
    this.value.response_body_bytes += responseBytes;
    increment(this.value.methods, method);
    increment(this.value.operations, op);
    increment(this.value.categories, group);
    increment(this.value.classes, requestClass);
    increment(this.value.class_request_body_bytes, requestClass, requestBytes);
    increment(this.value.class_response_body_bytes, requestClass, responseBytes);
    increment(this.value.statuses, Math.floor(status / 100) + 'xx');
    if (error) increment(this.value.proxy_errors, error);
    const bins = this.value.latency_ms_histogram[op] ||= Object.create(null);
    increment(bins, elapsedMs);
  }
}

async function serveWorker(config) {
  const counter = new Counter(config.key_prefix);
  const upstream = new URL(config.upstream);
  if (upstream.protocol !== 'http:' || upstream.hostname !== '127.0.0.1') throw Error('Qualification upstream must be local HTTP');
  // Active requests receive their own 60-second deadline. Free sockets expire
  // before RustFS's five-second header deadline; there is no automatic retry.
  const agent = new http.Agent({ keepAlive: true, maxSockets: Infinity, maxFreeSockets: 256, timeout: 1000 });
  const hop = new Set(['connection', 'proxy-connection', 'keep-alive', 'transfer-encoding', 'te', 'trailer', 'upgrade']);
  const transportHeaders = headers => {
    // Keep header names as data and preserve duplicate-header arrays while
    // avoiding per-header entry tuples on every provider request.
    const forwarded = Object.create(null);
    for (const name of Object.keys(headers)) {
      if (!hop.has(name)) forwarded[name] = headers[name];
    }
    return forwarded;
  };
  let active = 0;
  const server = http.createServer({ keepAliveTimeout: 65000, headersTimeout: 70000, requestTimeout: 0 }, (incoming, outgoing) => {
    incoming.socket.setNoDelay(true);
    const started = process.hrtime.bigint();
    let recorded = false, requestBytes = 0, responseBytes = 0, status = 502;
    active++;
    const record = error => {
      if (recorded) return;
      recorded = true;
      active--;
      const declared = incoming.headers['content-length'];
      counter.record(incoming.method, incoming.url, incoming.headers, declared === undefined ? requestBytes : Number(declared), responseBytes, status, Math.round(Number(process.hrtime.bigint() - started) / 1e6), error);
    };
    const fail = error => {
      record(error.code || 'transport_error');
      if (outgoing.headersSent) outgoing.destroy();
      else {
        outgoing.writeHead(502, { 'content-type': 'text/plain', 'connection': 'close' });
        outgoing.end('Qualification provider transport failed');
      }
    };
    const request = http.request({ hostname: upstream.hostname, port: upstream.port, method: incoming.method, path: upstream.pathname.replace(/\/$/, '') + incoming.url, headers: transportHeaders(incoming.headers), agent, timeout: 60000 }, response => {
      status = response.statusCode;
      outgoing.writeHead(status, transportHeaders(response.headers));
      response.on('data', chunk => { responseBytes += chunk.length; });
      response.on('error', fail);
      response.on('end', () => record(null));
      response.pipe(outgoing);
    });
    request.on('socket', socket => socket.setNoDelay(true));
    request.on('error', fail);
    request.on('timeout', () => request.destroy(Object.assign(Error('Upstream deadline'), { code: 'upstream_timeout' })));
    incoming.on('data', chunk => { requestBytes += chunk.length; });
    incoming.on('error', error => request.destroy(error));
    incoming.on('aborted', () => request.destroy(Object.assign(Error('Client aborted'), { code: 'client_aborted' })));
    outgoing.on('close', () => { if (!recorded && !outgoing.writableFinished) request.destroy(Object.assign(Error('Client closed'), { code: 'client_closed' })); });
    incoming.pipe(request);
  });
  const snapshot = () => ({ counter: counter.value, active_requests_at_shutdown: active,
    peak_rss_bytes: process.resourceUsage().maxRSS * 1024, pid: process.pid });
  let stop;
  const stopping = new Promise(resolve => { stop = resolve; });
  process.on('message', message => {
    if (message.kind === 'snapshot') process.send({ id: message.id, value: snapshot() });
    if (message.kind === 'stop') stop(message.id);
  });
  process.once('SIGTERM', () => stop(null));
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const id = await stopping;
  await new Promise(resolve => server.close(resolve));
  agent.destroy();
  await new Promise((resolve, reject) => process.send({ id, value: snapshot() }, error => error ? reject(error) : resolve()));
  process.disconnect();
  if (active !== 0) process.exitCode = 1;
}

function sumCounters(counters) {
  const total = Object.create(null);
  for (const counter of counters) {
    for (const [key, value] of Object.entries(counter)) {
      total[key] = typeof value === 'number' ? (total[key] || 0) + value
        : sumCounters([total[key] || {}, value]);
    }
  }
  return total;
}

async function serve(config) {
  // Two workers share one endpoint identity. A single JavaScript thread must
  // not serialize both replicas' provider traffic during throughput tests.
  cluster.schedulingPolicy = cluster.SCHED_RR;
  const workers = [cluster.fork(), cluster.fork()];
  const exits = workers.map(worker => once(worker, 'exit'));
  let stop, failed = false, closing = false, sequence = 0;
  const stopping = new Promise(resolve => { stop = resolve; });
  for (const worker of workers) worker.once('exit', () => { if (!closing) failed = true; stop(); });
  process.once('SIGTERM', stop);
  const query = (worker, kind) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const finish = (error, value) => {
      clearTimeout(timer);
      worker.off('message', receive);
      worker.off('exit', exited);
      error ? reject(error) : resolve(value);
    };
    const receive = message => { if (message.id === id) finish(null, message.value); };
    const exited = () => finish(Error('Measurement worker exited'));
    const timer = setTimeout(() => finish(Error('Measurement worker deadline')), kind === 'stop' ? 65000 : 10000);
    worker.on('message', receive);
    worker.once('exit', exited);
    worker.send({ id, kind }, error => { if (error) finish(error); });
  });
  let control, service;
  try {
    const listening = await Promise.race([
      Promise.all(workers.map(worker => once(worker, 'listening'))),
      Promise.race(exits).then(() => { throw Error('Measurement worker failed to start'); }),
    ]);
    const port = listening[0][0].port;
    if (listening.some(([address]) => address.port !== port)) throw Error('Measurement endpoints differ');
    control = http.createServer(async (request, response) => {
      if (request.url !== '/provider') { response.writeHead(404); response.end(); return; }
      try {
        const values = await Promise.all(workers.map(worker => query(worker, 'snapshot')));
        const body = JSON.stringify(sumCounters(values.map(value => value.counter)));
        response.writeHead(200, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body) });
        response.end(body);
      } catch {
        failed = true;
        response.writeHead(503);
        response.end();
        stop();
      }
    });
    await new Promise(resolve => control.listen(0, '127.0.0.1', resolve));
    service = { status: 'running', pid: process.pid, worker_pids: workers.map(worker => worker.process.pid),
      meter_url: 'http://127.0.0.1:' + port, provider_url: 'http://127.0.0.1:' + control.address().port + '/provider',
      node: process.version, upstream: config.upstream,
      scope: 'Two streaming measurement workers; no fault injection, caching or retry. Bounded stream buffers and aggregate counters; no credentials or paths in observations.' };
    fs.writeFileSync(config.services, JSON.stringify(service, null, 2) + '\n');
    console.log('Native measurement relay ready');
    await stopping;
  } finally {
    closing = true;
    if (control) await new Promise(resolve => control.close(resolve));
    // Missing worker totals invalidate the run; never publish partial counters.
    const values = await Promise.allSettled(workers.map(worker => query(worker, 'stop')));
    for (let index = 0; index < workers.length; index++) {
      if (values[index].status === 'rejected' && !workers[index].isDead()) workers[index].kill('SIGKILL');
    }
    const codes = await Promise.all(exits);
    const complete = values.every(value => value.status === 'fulfilled');
    if (complete) {
      const snapshots = values.map(value => value.value);
      fs.writeFileSync(config.final, JSON.stringify({ ...sumCounters(snapshots.map(value => value.counter)),
        active_requests_at_shutdown: snapshots.reduce((sum, value) => sum + value.active_requests_at_shutdown, 0),
        peak_rss_bytes: process.resourceUsage().maxRSS * 1024 + snapshots.reduce((sum, value) => sum + value.peak_rss_bytes, 0),
        workers: snapshots }, null, 2) + '\n');
    }
    failed ||= !complete || codes.some(([code]) => code !== 0);
    if (service) {
      service.status = failed ? 'failed' : 'stopped';
      fs.writeFileSync(config.services, JSON.stringify(service, null, 2) + '\n');
    }
    if (failed) process.exitCode = 1;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const config = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
  await (cluster.isPrimary ? serve(config) : serveWorker(config));
}
