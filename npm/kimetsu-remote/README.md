# kimetsu-remote (beta)

Server-hosted Kimetsu brain over **HTTP MCP** — one brain per repository, shared
from a server. This is a **separate** package from the `kimetsu-ai` CLI: the
remote server is intentionally not installed with `kimetsu`.

```bash
npm install -g kimetsu-remote
export KIMETSU_MCP_ENABLE_WRITE_TOOLS=1  # opt in to authenticated memory writes
kimetsu-remote serve --addr 127.0.0.1:8787 --data /srv/kimetsu-brains --token <secret>
```

Release binaries built with `--features embeddings,tls,stores` include SQLite,
PostgreSQL, Redis and DynamoDB checkpoint storage, and are published for
Linux x64, macOS Apple Silicon, and Windows x64. Elsewhere, install from source:

```bash
cargo install kimetsu-remote --features embeddings,tls,stores
```

For a disposable HTTP instance, configure an external durable store:

```bash
export KIMETSU_REMOTE_STORAGE=postgres
export KIMETSU_REMOTE_DATABASE_URL='postgresql://USER:PASSWORD@DB/kimetsu?sslmode=require'
export KIMETSU_REMOTE_STORAGE_NAMESPACE=team
export KIMETSU_REMOTE_TOKEN='<secret>'
export KIMETSU_MCP_ENABLE_WRITE_TOOLS=1
kimetsu-remote serve --addr 127.0.0.1:8787
```

Place a TLS proxy in front of the loopback listener. Each request reconstructs a
temporary SQLite retrieval projection; the store holds durable checkpoints.
`--shared-org-brain` shares org context across every repo in the namespace.
Shared-memory writes require a global token; repo tokens can read shared context.
Enable sharing only inside one trust group.
Write tools are disabled by default. Set `KIMETSU_MCP_ENABLE_WRITE_TOOLS=1` in the
server process to allow authenticated clients to record/curate memories; leave it
unset or set it to `0` for read-only use. Health checks do not verify this opt-in.

Source builds can select just `postgres` or `redis`. `dynamodb` and the `stores`
umbrella require Rust **1.91.1+**; SQLite needs no storage feature. `lambda` builds
the separate `kimetsu-remote-lambda` custom-runtime binary.
See [storage configuration and limits](https://github.com/RodCor/kimetsu/blob/main/docs/remote-storage.md)
and [Compose / SAM examples](https://github.com/RodCor/kimetsu/tree/main/deploy/remote-stateless).

> **Beta.** Under active testing; expect rough edges or breaking changes before
> the stable release. Put a TLS proxy in front (or use `--tls-cert`/`--tls-key`),
> and see the main [README](https://github.com/RodCor/kimetsu#readme) for the
> full deploy + client-wiring guide (`kimetsu plugin install --remote`).

Licensed under MIT OR Apache-2.0.
