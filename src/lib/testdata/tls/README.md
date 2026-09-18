These are public test fixtures, not deployment credentials. `server-key.der`
is an unencrypted PKCS#8 key used only by the loopback TLS tests. The server
certificate is signed by `ca.der`, has DNS SAN `localhost`, and is valid from
September 2026 through August 2126. This CA is never trusted by production ngit.
