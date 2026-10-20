import { Container } from '@cloudflare/containers';
import { createRemoteJWKSet, jwtVerify } from 'jose';

let keys;
export class CrabContainer extends Container {
  defaultPort = 8080;
  sleepAfter = '30m';
  constructor(ctx, env) {
    super(ctx, env);
    this.envVars = {
      AWS_ACCESS_KEY_ID: env.R2_ACCESS_KEY_ID,
      AWS_SECRET_ACCESS_KEY: env.R2_SECRET_ACCESS_KEY,
      AWS_ENDPOINT_URL_S3: env.R2_ENDPOINT,
      AWS_REGION: 'auto',
      AWS_DEFAULT_REGION: 'auto',
      AWS_VIRTUAL_HOSTED_STYLE_REQUEST: 'false',
      AWS_EC2_METADATA_DISABLED: 'true',
      CRAB_CONFIG_B64: env.CRAB_CONFIG_B64,
      CRAB_CA_B64: env.CRAB_CA_B64,
      CRAB_CERT_B64: env.CRAB_CERT_B64,
      CRAB_KEY_B64: env.CRAB_KEY_B64,
      RUST_LOG: 'crab_server=info,cellar=warn',
    };
  }
  async onStop() {
    this.readiness = undefined;
  }
  async waitUntilReady() {
    await this.startAndWaitForPorts({
      cancellationOptions: { portReadyTimeoutMS: 90_000 },
    });
    for (let attempt = 0; attempt < 90; attempt++) {
      try {
        const response = await this.containerFetch('http://container/api/repos');
        await response.body?.cancel();
        if (response.ok) return;
      } catch {}
      await new Promise(resolve => setTimeout(resolve, 1000));
    }
    throw new Error('Crab did not become ready');
  }
  async fetch(request) {
    this.readiness ??= this.waitUntilReady().catch(error => {
      this.readiness = undefined;
      throw error;
    });
    await this.readiness;
    return this.containerFetch(request);
  }
}

export default {
  async fetch(request, env) {
    const assertion = request.headers.get('cf-access-jwt-assertion');
    if (!assertion) return new Response('Sign in through Cloudflare Access', { status: 401 });
    const issuer = `https://${env.ACCESS_TEAM}`;
    keys ??= createRemoteJWKSet(new URL(`${issuer}/cdn-cgi/access/certs`));
    let identity;
    try {
      identity = (await jwtVerify(assertion, keys, {
        issuer, audience: env.ACCESS_AUD, algorithms: ['RS256'],
      })).payload;
    } catch {
      return new Response('Invalid access session', { status: 401 });
    }
    const url = new URL(request.url);
    const safe = ['GET', 'HEAD', 'OPTIONS'].includes(request.method);
    const origin = request.headers.get('origin');
    const browserRequest = origin !== null || request.headers.has('sec-fetch-site');
    if (!safe && identity.email && browserRequest && origin !== url.origin) {
      return new Response('Invalid request origin', { status: 403 });
    }
    const instance = env.CRAB.getByName('repository-server');
    if (url.pathname === '/__cloudflare/status') return Response.json(await instance.getState());
    if (url.pathname === '/__cloudflare/restart') {
      if (request.method !== 'POST' || !identity.common_name) return new Response('Forbidden', { status: 403 });
      await instance.stop('SIGTERM');
      return Response.json({ status: 'stopping' }, { status: 202 });
    }
    const headers = new Headers(request.headers);
    for (const name of ['cf-access-jwt-assertion', 'cf-access-client-id', 'cf-access-client-secret']) headers.delete(name);
    const response = await instance.fetch(new Request(request, { headers }));
    if (response.ok && request.method === 'POST' && /^\/git\/[^/]+\/[^/]+\/info\/lfs\/objects\/batch$/.test(url.pathname)) {
      const batch = await response.json();
      // The proxy owns both directions of the public-to-loopback authority mapping.
      // LFS follows absolute action URLs; leaving loopback URLs breaks remote clients.
      for (const object of batch.objects) {
        for (const action of Object.values(object.actions ?? {})) {
          const target = new URL(action.href);
          if (target.origin !== 'http://localhost:8788') return new Response('Invalid LFS action origin', { status: 502 });
          action.href = url.origin + target.pathname + target.search;
        }
      }
      const responseHeaders = new Headers(response.headers);
      responseHeaders.delete('content-length');
      return Response.json(batch, { status: response.status, headers: responseHeaders });
    }
    return response;
  },
};
