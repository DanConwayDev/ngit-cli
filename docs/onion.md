# Tor / `.onion` support

ngit can talk to nostr relays and grasp git servers hosted on `.onion`
addresses. This is useful when a maintainer wants to publish a repo behind a
Tor hidden service, or a contributor wants to push to a hidden grasp server.

## Quick start

1. Run a Tor SOCKS5 proxy on the local machine. The Tor Browser bundle, the
   `tor` package on Linux/macOS, or anything else that exposes a SOCKS5 listener
   will do. The default Tor port is `9050`.
2. Use `.onion` hosts wherever ngit accepts a relay or grasp server URL:

   ```sh
   git clone nostr://npub1.../<onion-host>/<repo-identifier>
   ```

   Example using the public ngit-grasp hidden service:

   ```sh
   git clone nostr://npub1gvv9ahktvavf9qjtrgm62le7gplmmchd5usp5wpfhr85hf79kncqj8xchs/nkkkrgkv3pov3hibjo7kjnc7raslwaqqvmvtzqy2mbsa7liqov6l5qid.onion/0xchat-app-main
   ```

That's it. ngit detects the `.onion` suffix and routes only that traffic
through the Tor SOCKS5 proxy. Clearnet relays and clearnet clone URLs keep
talking directly to the network.

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
| `NGIT_TOR_PROXY` | `127.0.0.1:9050` | SOCKS5 address used for `.onion` traffic. Set to `none` / `off` / `disabled` (or empty) to disable routing `.onion` traffic through a SOCKS5 proxy; `.onion` connections will then go through libgit2's / nostr-sdk's default routing, which generally fails on hosts without a transparent Tor proxy. |

The same env var controls both the nostr-sdk relay proxy and the libgit2
proxy. There is no per-relay / per-server override — if you need that, file
an issue.

## What ngit deliberately does **not** do

- It does **not** ship an embedded Tor client. You bring your own Tor SOCKS5
  proxy. This keeps the binary small and lets the OS handle Tor lifecycle.
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

- `connection refused` on the SOCKS5 port: check that Tor is running and
  listening on `127.0.0.1:9050` (or whatever you set `NGIT_TOR_PROXY` to).
  `ss -lntp | grep 9050` or `lsof -i :9050` will tell you.
- `Couldn't resolve host` from libgit2 when cloning a `.onion` URL: this
  usually means libgit2 isn't using SOCKS5h and is trying to resolve the
  onion name locally. Confirm the URL host is `.onion` and the env var
  resolves to a usable SOCKS5 endpoint.
- The relay accepts connections but the kind:30617 announcement isn't found:
  the relay-hint segment in the `nostr://` URL must match exactly what the
  maintainer published in their announcement event's `clone` / `relays` tags.
