import http from 'node:http';
import fs from 'node:fs';
import { pathToFileURL } from 'node:url';

export function operation(method, path, headers) {
  const query = new URL(path, 'http://meter').searchParams;
  if (method === 'HEAD') return 'head';
  if (method === 'GET') return query.has('list-type') || query.has('versions') ? 'list' : 'get';
  if (method === 'PUT') {
    if ('x-amz-copy-source' in headers) return 'copy';
    return query.has('partNumber') ? 'multipart_part' : 'put';
  }
  if (method === 'POST') {
    if (query.has('uploads')) return 'multipart_create';
    if (query.has('uploadId')) return 'multipart_complete';
    return query.has('delete') ? 'delete_batch' : 'post';
  }
  if (method === 'DELETE') return query.has('uploadId') ? 'multipart_abort' : 'delete';
  return method.toLowerCase();
}

export function requestKey(path, op) {
  const parsed = new URL(path, 'http://meter');
  const key = decodeURIComponent(parsed.pathname).replace(/^\/+/, '');
  const prefix = parsed.searchParams.get('prefix');
  return op === 'list' && prefix ? key.replace(/\/+$/, '') + '/' + prefix.replace(/^\/+/, '') : key;
}

export function category(key) {
  const parts = key.split('/');
  if (parts.length > 2 && parts[0] === 'metadata' && ['pack', 'shard'].includes(parts[1])) return parts.slice(0, 3).join('/');
  if (parts.length > 2 && parts[0] === 'locks' && parts[1] === 'internal') return parts.slice(0, 3).join('/');
  if (parts.length > 1 && ['git_object_catalog_db', 'v2', 'git-visibility', 'locks', 'manifests', 'metadata', 'ref-journal', 'refs'].includes(parts[0])) return parts.slice(0, 2).join('/');
  return parts[0] || 'repository-root';
}

export class Counter {
  constructor(prefix) {
    this.prefix = prefix.replace(/^\/+|\/+$/g, '') + '/';
    this.value = { requests: 0, request_body_bytes: 0, response_body_bytes: 0 };
    for (const field of ['methods', 'operations', 'categories', 'classes', 'class_request_body_bytes', 'class_response_body_bytes', 'statuses', 'proxy_errors', 'latency_ms_histogram']) this.value[field] = Object.create(null);
  }
  record(method, path, headers, requestBytes, responseBytes, status, elapsedMs, error) {
    const op = operation(method, path, headers);
    const key = requestKey(path, op);
    const group = key.startsWith(this.prefix) ? category(key.slice(this.prefix.length)) : 'outside-repository';
    const requestClass = group + ':' + op;
    this.value.requests++;
    this.value.request_body_bytes += requestBytes;
    this.value.response_body_bytes += responseBytes;
    for (const [field, name, count] of [['methods', method, 1], ['operations', op, 1], ['categories', group, 1], ['classes', requestClass, 1], ['class_request_body_bytes', requestClass, requestBytes], ['class_response_body_bytes', requestClass, responseBytes], ['statuses', Math.floor(status / 100) + 'xx', 1]]) this.value[field][name] = (this.value[field][name] || 0) + count;
    if (error) this.value.proxy_errors[error] = (this.value.proxy_errors[error] || 0) + 1;
    const bins = this.value.latency_ms_histogram[op] ||= Object.create(null);
    bins[elapsedMs] = (bins[elapsedMs] || 0) + 1;
  }
}

async function serve(config) {
  const counter = new Counter(config.key_prefix);
  const upstream = new URL(config.upstream);
  if (upstream.protocol !== 'http:' || upstream.hostname !== '127.0.0.1') throw Error('Qualification upstream must be local HTTP');
  // Active requests receive their own 60-second deadline. Free sockets expire
  // before RustFS's five-second header deadline; there is no automatic retry.
  const agent = new http.Agent({ keepAlive: true, maxSockets: Infinity, maxFreeSockets: 256, timeout: 1000 });
  const hop = new Set(['connection', 'proxy-connection', 'keep-alive', 'transfer-encoding', 'te', 'trailer', 'upgrade']);
  const transportHeaders = headers => Object.fromEntries(Object.entries(headers).filter(([name]) => !hop.has(name)));
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
  const control = http.createServer((request, response) => {
    if (request.url !== '/provider') { response.writeHead(404); response.end(); return; }
    const body = JSON.stringify(counter.value);
    response.writeHead(200, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body) });
    response.end(body);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  await new Promise(resolve => control.listen(0, '127.0.0.1', resolve));
  const service = { status: 'running', pid: process.pid, meter_url: 'http://127.0.0.1:' + server.address().port, provider_url: 'http://127.0.0.1:' + control.address().port + '/provider', node: process.version, upstream: config.upstream, scope: 'Streaming measurement relay only; no fault injection, caching or retry. Bounded stream buffers and aggregate counters; no credentials or paths in observations.' };
  fs.writeFileSync(config.services, JSON.stringify(service, null, 2) + '\n');
  console.log('Native measurement relay ready');
  await new Promise(resolve => process.once('SIGTERM', resolve));
  await Promise.all([new Promise(resolve => server.close(resolve)), new Promise(resolve => control.close(resolve))]);
  agent.destroy();
  fs.writeFileSync(config.final, JSON.stringify({ ...counter.value, active_requests_at_shutdown: active, peak_rss_bytes: process.resourceUsage().maxRSS * 1024 }, null, 2) + '\n');
  service.status = 'stopped';
  fs.writeFileSync(config.services, JSON.stringify(service, null, 2) + '\n');
  if (active !== 0) process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await serve(JSON.parse(fs.readFileSync(process.argv[2], 'utf8')));
