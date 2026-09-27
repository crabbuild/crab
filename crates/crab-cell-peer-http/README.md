# Crab Cell peer HTTP transport

`PeerHttpRoundTrip` sends authenticated Cell peer envelopes to the current
enrolled owner. It reloads the authority and node directory on a retry, bounds
responses, distinguishes an unknown outcome from a request that never started,
and caches HTTP clients by enrolled session, certificate, and public key.

Owner-routed requests make at most two attempts. On HTTP 429/503 with an integer
`Retry-After` header, the transport paces the retry within the original deadline,
then reloads ownership. Persistent admission rejection returns a capacity error.
A delay that leaves no request budget returns a deadline error without sleeping.
Responses without that header retain the immediate owner-refresh behavior used
by existing receivers. Lost or invalid responses remain unknown outcomes and
are not retried here. Direct-node activation remains a single attempt: its
caller owns scheduling and retries, and receives the same capacity distinction.

The product supplies `PeerTargetScope` and `PeerHttpClientFactory`. The factory
must authenticate its local client identity and pin the remote certificate and
public key passed to it. The receiver must verify the peer envelope, enrollment,
and product authorization before dispatching through `PeerDispatcher`.

BeyondDB composes this transport and its pinned-mTLS implementation for account,
credential, data, and coordinator Cells. Its incoming receiver reports a
one-second retry delay when request-memory admission is exhausted; codec work
waits within the request deadline using the runtime's bounded CPU queue. `crab-http-server`
retains its repository-scoped transport, owner-description hints, and telemetry;
those product routing semantics are not enabled by this transport.
