# Stateless remote storage

Kimetsu Remote keeps its repository-scoped HTTP MCP API. The selected store is
its durable authority; the SQLite database reconstructed for each request is a
disposable retrieval projection. With an external store, losing the process or
its temporary directory does not delete committed state.

## Select a storage mode

Existing `serve --data /srv/kimetsu-brains` remains local-file mode and requires a
persistent filesystem. Explicit `--storage` selects checkpoint mode. Omit `--data`
in checkpoint mode: it is not the state location. The SQLite checkpoint store
itself still needs a persistent filesystem.

| Backend | Build feature | Connection setting |
| --- | --- | --- |
| SQLite | Always available | `KIMETSU_REMOTE_DATABASE_URL=/persistent/checkpoints.sqlite` (also accepts `sqlite://` prefix) |
| PostgreSQL | `postgres` | `KIMETSU_REMOTE_DATABASE_URL=postgresql://USER:PASSWORD@HOST/DB?sslmode=require` |
| Redis | `redis` | `KIMETSU_REMOTE_DATABASE_URL=rediss://USER:PASSWORD@HOST:6379/0` |
| DynamoDB | `dynamodb` | `KIMETSU_REMOTE_DYNAMODB_TABLE=TABLE`, AWS region and credentials |

`stores` enables all optional providers. Selecting a provider absent from the
build fails startup. DynamoDB dependencies, including through `stores`, require
Rust **1.91.1+**; this optional requirement does not raise the lean workspace's
declared Rust requirement. Release HTTP binaries use `embeddings,tls,stores`.
The separate Lambda binary needs `lambda` plus the selected provider.

```bash
export KIMETSU_REMOTE_STORAGE=postgres
export KIMETSU_REMOTE_DATABASE_URL='postgresql://USER:PASSWORD@DB/kimetsu?sslmode=require'
export KIMETSU_REMOTE_STORAGE_NAMESPACE=team
export KIMETSU_REMOTE_TOKEN='<secret>'
export KIMETSU_MCP_ENABLE_WRITE_TOOLS=1  # opt in to authenticated memory writes
kimetsu-remote serve --addr 127.0.0.1:8787
```

This listener expects a local TLS reverse proxy. Direct HTTPS requires `tls` and
`--tls-cert CHAIN.pem --tls-key KEY.pem`. A non-loopback plaintext bind requires
explicit `--allow-public-http`; use it only on a trusted network, such as the
private container network in the [deployment examples](../deploy/remote-stateless/README.md).
Prefer secret injection through the environment over credential-bearing command
arguments. Provider errors and connection Debug output are redacted; surrounding
scripts and proxies must also avoid logging secrets.

## Configuration

CLI values take precedence over their environment equivalents. Flags without an
environment entry must be passed as arguments, including for the Lambda binary.
Namespaces are 1-64 ASCII letters, digits, dots, hyphens or underscores.

| Flag | Environment | Default / meaning |
| --- | --- | --- |
| `--storage sqlite\|postgres\|redis\|dynamodb` | `KIMETSU_REMOTE_STORAGE` | Unset means local `--data` mode; `postgresql` also accepted |
| `--database-url` | `KIMETSU_REMOTE_DATABASE_URL` | Required for SQLite, PostgreSQL, Redis |
| `--storage-namespace` | `KIMETSU_REMOTE_STORAGE_NAMESPACE` | `default`; separate deployments/trust groups |
| `--dynamodb-table` | `KIMETSU_REMOTE_DYNAMODB_TABLE` | Required for DynamoDB; table must already exist |
| `--aws-region` | `AWS_REGION`, then `AWS_DEFAULT_REGION` | Standard SDK region resolution if absent |
| `--dynamodb-endpoint` | `KIMETSU_REMOTE_DYNAMODB_ENDPOINT` | Unset for AWS; operator-owned emulator override |
| `--shared-org-brain` | `KIMETSU_REMOTE_SHARED_ORG` | Off; env accepts `true/1/on` or `false/0/off` |
| Repeatable `--token` | `KIMETSU_REMOTE_TOKEN` | Auth source; env is comma-separated |
| `--tokens-file` | None | TOML global/per-repo tokens and optional names |
| `--rate-limit` | None | 60 requests/token/minute; 0 disables |
| `--reranker` | None | `jina-reranker-v1-tiny-en`; `off` disables |
| None (environment only) | `KIMETSU_MCP_ENABLE_WRITE_TOOLS` | Off; set `1` in the server process to enable authenticated write tools |

Write tools are disabled by default, even when storage and bearer auth are
configured. Set `KIMETSU_MCP_ENABLE_WRITE_TOOLS=1` on each HTTP/Lambda instance
that should record or curate memories; unset or `0` keeps them disabled. A healthy
empty deployment can therefore serve requests while offering no memory writes.
The Compose templates pass this flag through with default `0`; the SAM template
uses `EnableWriteTools=1` as the explicit operator opt-in. Enabling the gate does
not bypass repo authorization or global-token checks for shared-memory writes.

AWS credentials follow the SDK chain in production; prefer the Lambda execution
role. Never supply live AWS credentials to emulator tests. An endpoint override
is operator configuration, not an MCP parameter. Storage initialization is
bounded to 20 seconds and fails without a local fallback.

## Auth and shared org memory

Repository ids retain their validation and bearer-token checks. For scoped
access, use `--tokens-file`:

```toml
global = ["ADMIN_SECRET"]
[per_repo]
acme-api = ["API_SECRET"]
acme-web = ["WEB_SECRET"]
```

Instances sharing a deployment must use the same namespace, token policy and rate
limit. Namespaces are operator boundaries, not a replacement for authentication
or database permissions separating unrelated tenants.

`--shared-org-brain` (or `KIMETSU_REMOTE_SHARED_ORG=true`) stores one org checkpoint
shared by **every repo in that namespace**. Authorized repo clients can retrieve
its context. Shared org/user-memory writes require a global token; repo tokens
cannot write that scope. Enable sharing only inside one trust group. Use separate
namespaces and credentials for organizations needing isolation. Checkpoint mode
rejects `--org-brain PATH`; local mode retains that option. Managed ingest remains
opt-in through `--repos-file` and `--checkout-dir`; its checkouts are separate
operational data and must be available wherever ingest runs.

`kimetsu_brain_conflict_resolve` also requires a global token, including project
resolutions: its current resolution path can affect inherited org conflicts.

## Checkpoints, concurrency and limits

Each request loads current repo and optional org records, reconstructs private
SQLite databases, invokes the existing allowlisted tool, and publishes changed
checkpoints against their expected revisions before responding successfully.
Repo and org changes publish together. Conflicts rerun from fresh state a bounded
number of times. Persistent contention or unavailable storage returns a failure.
A retry after a transport failure can still repeat an operation: there is no
client-request deduplication promise. No sticky sessions are required. Rate
limits are atomic fixed-minute counters in the shared store. Metrics remain per
process and need aggregation across instances.

The portable format holds structured table records and configuration (MessagePack
compressed with gzip), including events, embeddings, graph and feedback. It is
not a raw SQLite file copy. FTS/shadow tables and caches are rebuilt. Checkpoint
format **1** must match the binary's exact brain schema. Incompatible formats or
schemas fail explicitly and need an explicit migration. Back up the durable store
before changing schema versions. There is no online cross-version migration or
automatic import of existing `--data` directories. Selecting an empty checkpoint
namespace starts empty. Roll back to a schema-compatible binary, or restore a
compatible backup with all writers stopped.

Each checkpoint is bounded to **64 MiB compressed**, **256 MiB decoded**, and
**2,000,000 rows**. The store API allows at most four checkpoint writes in one
atomic batch. Invalid, corrupt, oversized or incompatible records fail without
fallback. Whole checkpoints are loaded and written, so cost grows with brain
size. DynamoDB uses chunks of at most 300,000 bytes (up to 224 per maximum-sized
checkpoint), strong reads and transactional conditional head publication.
Sequential chunk traffic bounds concurrency but raises latency and request cost.
Retrieval runs through the existing SQLite engine; no native-database or
constant-time performance claim is made.

Size RAM, writable temporary space, timeouts and concurrency with representative
brains. `/tmp` must hold decoded projections and rebuilt indexes for simultaneous
requests. Cold starts also pay model loading/download costs in semantic builds;
model caches are disposable and never authoritative. Package model assets or
provide initialization network access as needed. Lambda and API Gateway payload
and time limits are independent of checkpoint limits.

## Provider operations

**SQLite:** use an ordinary persistent file, not `:memory:` or a SQLite URI.
The adapter uses WAL, FULL synchronous mode and transactional writes. Do not
share the file across hosts on an unsuitable network filesystem.

**PostgreSQL:** startup creates `kimetsu_checkpoints`, `kimetsu_rate_limits` and a
rate-expiry index if absent. The role needs schema CREATE and table/index
privileges for initialization and SELECT/INSERT/UPDATE/DELETE at runtime. Writes
use transactions and row locks. Require TLS with `sslmode=require`, a valid
server hostname and an OS-trusted CA; the native TLS connector verifies the
certificate. Install a private CA in the container trust store when needed.
The local Compose fixture disables TLS only inside its isolated network. Size
connection capacity for the number of instances.

**Redis:** deploy it as a durable database. Configure `appendonly yes`,
`appendfsync always`, `maxmemory-policy noeviction`, persistent storage, backups
and adequate memory/disk. `everysec` permits loss of recent acknowledged writes;
RDB snapshots alone do not satisfy this durability policy. Lua validates expected
revisions before a single HSET publication. Durable checkpoint hashes have no
TTL; only separate rate keys expire. The adapter does not rewrite service-wide
policies. Use `rediss://` and trusted certificates outside local fixtures.
Evaluate managed-service replication and failover separately: local AOF fsync
cannot guarantee a replica has every acknowledged write. See
[Redis persistence](https://redis.io/docs/latest/operate/oss_and_stack/management/persistence/).

**DynamoDB:** provision string partition key **`pk`** and string sort key **`sk`**;
no secondary index is needed. Enable TTL on numeric **`expires_at`**, written
**only to rate buckets**. Never expire checkpoint heads/chunks. TTL deletion is
asynchronous; bucket identity separates minutes without waiting for deletion.
See [AWS TTL behavior](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/TTL.html).
Use one regional table and strongly consistent reads; no multi-region global-table
conflict-resolution guarantee is provided. Grant table-scoped `GetItem`,
`PutItem`, `UpdateItem`, `DeleteItem` and `ConditionCheckItem` permissions for
transactional item actions. Production runtime needs no Create/Delete/DescribeTable,
Scan or Query. The [SAM example](../deploy/remote-stateless/lambda/template.yaml)
provisions a table and scoped role only if an operator deploys it.

DynamoDB chunks are immutable; heads publish atomically. Best-effort cleanup can
leave failed-staging/crash orphans. There is **no background garbage collector**.
For an orphan sweep, stop all writers, wait for in-flight operations, read current
manifests, retain every referenced chunk and delete only unreferenced chunks.
An unreferenced chunk can still belong to an active writer. Use a separate
maintenance role for Scan/Query and deletion; do not add TTL to durable chunks.

## Local verification

[Compose fixtures](../deploy/remote-stateless/README.md) include dedicated local
PostgreSQL and Redis. Normal tests do not infer service URLs or a cloud account.
Live tests are ignored unless selected with explicit dedicated test settings:

```bash
cargo test --locked -p kimetsu-remote
cargo test --locked -p kimetsu-remote --features stores,lambda,tls
KIMETSU_TEST_POSTGRES_URL='postgresql://kimetsu:LOCAL_PASSWORD@127.0.0.1:5432/kimetsu?sslmode=disable' \
  cargo test --locked -p kimetsu-remote --features postgres --test storage_contract postgres_ -- --ignored
KIMETSU_TEST_REDIS_URL='redis://127.0.0.1:6379/0' \
  cargo test --locked -p kimetsu-remote --features redis --test storage_contract redis_ -- --ignored
KIMETSU_TEST_DYNAMODB_ENDPOINT='http://127.0.0.1:8000' \
  cargo test --locked -p kimetsu-remote --features dynamodb local_emulator_contract --lib -- --ignored
```

Tests create unique namespaces. The DynamoDB Local contract creates/deletes a
temporary table and uses explicit dummy credentials. Its endpoint must be numeric
HTTP loopback. CI starts local PostgreSQL, Redis and DynamoDB Local; it does not
validate live AWS IAM, quotas, managed failover or cloud deployment. Deployment
examples are templates; tests do not deploy them.
