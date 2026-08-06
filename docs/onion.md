# Tor / `.onion` support

ngit can talk to nostr relays and grasp git servers hosted on `.onion`
addresses. This is useful when a maintainer wants to publish a repo behind a
Tor hidden service, or a contributor wants to push to a hidden grasp server.

## Quick start

1. Run a Tor SOCKS5 proxy on the local machine. A system Tor service commonly
   listens on `127.0.0.1:9050`; Tor Browser commonly uses `127.0.0.1:9150`.
   ngit probes both addresses and uses the first one available.
2. Use `.onion` hosts wherever ngit accepts a relay or grasp server URL:

   ```sh
   git clone nostr://npub1.../<onion-host>/<repo-identifier>
   ```

   Example using the public ngit-grasp hidden service:

   ```sh
   git clone nostr://npub1gvv9ahktvavf9qjtrgm62le7gplmmchd5usp5wpfhr85hf79kncqj8xchs/nkkkrgkv3pov3hibjo7kjnc7raslwaqqvmvtzqy2mbsa7liqov6l5qid.onion/0xchat-app-main
   ```

That's it. ngit opportunistically detects a running proxy and routes only
`.onion` traffic through it. Clearnet relays and clone URLs keep talking
directly. When no proxy is available, onion entries fail immediately instead
of delaying usable clearnet alternatives; an onion-only repository reports
how to enable Tor.

## What ngit does for `.onion` traffic

1. **Relay scheme**: a `.onion` host in a `nostr://...` URL (either as a
   path segment or via `?relay=...`) defaults to `ws://` instead of `wss://`,
   because hidden services don't terminate TLS.
2. **GRASP clone URL scheme**: `format_grasp_server_url_as_clone_url` /
   `format_grasp_server_url_as_grasp06_prs_url` default to `http://` for
   `.onion` hosts, for the same reason.
3. **GRASP relay URL scheme**: `format_grasp_server_url_as_relay_url` defaults
   to `ws://` for `.onion` hosts.
4. **Nostr-SDK relay client**: built with [`Proxy::onion(addr)`][proxy-onion],
   so `.onion` relay connections go through the configured SOCKS5 proxy and
   everything else stays direct.
5. **libgit2 fetch / list / push**: when the server URL host ends in `.onion`,
   ngit configures libgit2's `ProxyOptions` with `socks5h://<addr>`. The
   trailing `h` keeps DNS resolution at the proxy, which is mandatory for
   `.onion` addresses (the local resolver can't resolve them).

[proxy-onion]: https://docs.rs/nostr-sdk/latest/nostr_sdk/proxy/struct.Proxy.html#method.onion

## Configuration

| Env var | Default | Meaning |
| --- | --- | --- |
| `NGIT_TOR_PROXY` | auto-detect | SOCKS5 address used for `.onion` traffic. When unset, ngit probes `127.0.0.1:9050` then `127.0.0.1:9150`. Set an explicit `host:port` to probe only that address, or `none` / `off` / `disabled` (or empty) to disable onion routing. |

The same env var controls both the nostr-sdk relay proxy and the libgit2
proxy. There is no per-relay / per-server override — if you need that, file
an issue.

## What ngit deliberately does **not** do

- It does **not** ship or launch an embedded Tor client. ngit commands and the
  git remote helper are short-lived, so starting Tor from each process would
  repeatedly impose bootstrap cost and provide poor UX. ngit instead uses an
  already-running SOCKS5 proxy when one is available.
- It does **not** force every connection through Tor. Only `.onion` relay
  URLs and `.onion` clone URLs are proxied; clearnet stays direct. This is
  the standard "stream isolation by host" model.
- It does **not** proxy NIP-05 lookups (`https://<domain>/.well-known/nostr.json`).
  NIP-05 lookups still go through `reqwest`'s default transport. If you need
  this, file an issue.
- It does **not** validate Tor descriptors, hidden-service v3 cookies, or
  anything beyond "the host ends in `.onion`". Trust decisions about the
  hidden service itself are the operator's.

## Troubleshooting

- `no Tor SOCKS5 proxy is available`: check that Tor is running on
  `127.0.0.1:9050` or `127.0.0.1:9150`, or set `NGIT_TOR_PROXY` to its actual
  address.
- `Couldn't resolve host` from libgit2 when cloning a `.onion` URL: this
  usually means libgit2 isn't using SOCKS5h and is trying to resolve the
  onion name locally. Confirm the URL host is `.onion` and the env var
  resolves to a usable SOCKS5 endpoint.
- The relay accepts connections but the kind:30617 announcement isn't found:
  the relay-hint segment in the `nostr://` URL must match exactly what the
  maintainer published in their announcement event's `clone` / `relays` tags.
