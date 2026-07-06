# kimetsu-remote (beta)

Server-hosted Kimetsu brain over **HTTP MCP** — one brain per repository, shared
from a server. This is a **separate** package from the `kimetsu-ai` CLI: the
remote server is intentionally not installed with `kimetsu`.

```bash
npm install -g kimetsu-remote
kimetsu-remote serve --addr 0.0.0.0:8787 --data /srv/kimetsu-brains --token <secret>
```

Prebuilt binaries (built with `--features embeddings,tls`) are published for
Linux x64, macOS Apple Silicon, and Windows x64. Elsewhere, install from source:

```bash
cargo install kimetsu-remote --features embeddings
```

> **Beta.** Under active testing; expect rough edges or breaking changes before
> the stable release. Put a TLS proxy in front (or use `--tls-cert`/`--tls-key`),
> and see the main [README](https://github.com/RodCor/kimetsu#readme) for the
> full deploy + client-wiring guide (`kimetsu plugin install --remote`).

Licensed under MIT OR Apache-2.0.
