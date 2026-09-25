# Crab Cell peer HTTP transport

`PeerHttpRoundTrip` sends authenticated Cell peer envelopes to the current
enrolled owner. It reloads the authority and node directory on a retry, bounds
responses, distinguishes an unknown outcome from a request that never started,
and caches HTTP clients by enrolled session, certificate, and public key.

The product supplies `PeerTargetScope` and `PeerHttpClientFactory`. The factory
must authenticate its local client identity and pin the remote certificate and
public key passed to it. The receiver must verify the peer envelope, enrollment,
and product authorization before dispatching through `PeerDispatcher`.

`crab-http-server` provides the existing mTLS factory and private receiver.
BeyondDB now composes the same outgoing transport for its account, credential,
and data Cell namespaces; its incoming network peer service remains unfinished.
