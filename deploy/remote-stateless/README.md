# Stateless deployment examples

These examples keep checkpoints in a durable store and reconstruct disposable
SQLite retrieval projections per request. They are source templates; no cloud
resources are created until an operator runs a deployment command. See
[configuration, auth, schema compatibility and limits](../../docs/remote-storage.md).

## HTTP with Docker Compose

From the repository root, set your own local bearer token and a URL-safe local
database password (percent-encode special characters in connection URLs):

```bash
export KIMETSU_REMOTE_TOKEN='REPLACE_WITH_A_RANDOM_SECRET'
export POSTGRES_PASSWORD='REPLACE_WITH_A_URL_SAFE_RANDOM_PASSWORD'
export KIMETSU_MCP_ENABLE_WRITE_TOOLS=1  # opt in to authenticated memory writes
docker compose -f deploy/remote-stateless/http/compose.yaml up --build -d
curl http://127.0.0.1:8787/healthz
```

Both Compose templates pass `KIMETSU_MCP_ENABLE_WRITE_TOOLS` to the server with
default `0`. Export `1` as above to enable recording/curation, or leave it unset
for read-only use. A passing health check does not prove write tools are enabled.
The flag changes tool availability; bearer auth and repo/global-token scopes
still apply. Apply the same explicit choice to every instance in a deployment.

The default image builds `stores,tls` with no embedding models. To build semantic
retrieval, add `args: { FEATURES: 'embeddings,tls,stores' }` under `remote.build`,
and provision model assets/cache, runtime libraries and sufficient memory. The
default command disables the optional reranker. A lean build preserves lexical
and graph retrieval; it does not claim semantic parity with an embeddings build.

PostgreSQL has a named persistent volume. The HTTP container only needs its
writable `/tmp`; its 1 GiB tmpfs is a starting point, not a capacity guarantee.
PostgreSQL is reachable only on the Compose network. The local fixture disables
database TLS; production must require verified TLS and use managed secrets.
Only HTTP is published, on host loopback. `--allow-public-http` permits the
container-network listener; a TLS reverse proxy must front any public endpoint.
Keep `/metrics` private (it is unauthenticated), and forward `/mcp/<repo>` with
the bearer Authorization header intact. `/healthz` is a liveness probe and does
not prove the backend is currently writable.

For the standalone Redis alternative, use a different Compose project or stop
the PostgreSQL example first to free host port 8787:

```bash
docker compose -p kimetsu-redis -f deploy/remote-stateless/http/compose.redis.yaml up --build -d
```

Redis is private to this local network, with AOF, fsync on every write,
`noeviction` and a named data volume. Its fixture disables protected mode because
the network is isolated; do not publish its port or reuse that configuration on
a public/shared network. Production Redis needs authentication, verified TLS,
backups and a reviewed replication/failover durability policy. Checkpoint hashes
never expire; separate rate buckets do. Neither example deletes persistent data
when replacing the HTTP container. Pin images by digest for production and
review upgrades before applying them. Compose project volumes are authoritative
state: deleting them deletes that fixture's brains.

To use a managed PostgreSQL or Redis service, remove the database service and
`depends_on`, inject its TLS connection URL, and retain the remote container.
For DynamoDB, set `KIMETSU_REMOTE_STORAGE=dynamodb`, table, region and a workload
identity; omit the database URL. Never mount existing private AWS credential
files merely to run the examples.

The Compose databases intentionally have no host ports. Backend contract tests
need dedicated host-reachable fixtures and the explicit `KIMETSU_TEST_*`
variables in the [test instructions](../../docs/remote-storage.md#local-verification).
CI supplies those isolated fixtures automatically and also runs fresh-instance
HTTP tests. No test automatically targets cloud services.

## AWS Lambda custom runtime with SAM

The [SAM template](lambda/template.yaml) uses the existing
`kimetsu-remote-lambda` binary with `lambda,dynamodb`, an API Gateway HTTP API,
an on-demand DynamoDB table and a table-scoped execution role. It retains the
table on stack deletion/replacement and enables point-in-time recovery. Its
string `pk`/`sk` keys match the adapter; TTL on `expires_at` applies only to rate
items. Durable heads/chunks do not carry TTL. The template exposes MCP and
health routes; `/metrics` is not exposed through this API configuration.

Prerequisites: Rust **1.91.1+**, Cargo Lambda with its Zig build toolchain,
AWS SAM CLI, and an existing Secrets Manager secret whose JSON has a `token`
field. This token is a global/admin token. The deployer's CloudFormation role
must be allowed to resolve that secret (and its KMS key if applicable). The
function does not fetch Secrets Manager itself. Rotate/redeploy the function
configuration when the secret changes; a dynamic reference is not automatic
runtime rotation. Do not pass token plaintext as a SAM parameter.

Build the executable bootstrap from the repository root:

```bash
cargo lambda build --release --locked -p kimetsu-remote \
  --bin kimetsu-remote-lambda --features lambda,dynamodb
sam validate --lint --template-file deploy/remote-stateless/lambda/template.yaml
```

Cargo Lambda places an executable `bootstrap` in
`target/lambda/kimetsu-remote-lambda/`, which the template uses as `CodeUri`.
Use its default cross compiler to produce a Lambda-compatible Linux x86_64
binary; a native build on a newer glibc system may not run on Amazon Linux 2023.
If `CARGO_TARGET_DIR` is overridden, adjust `CodeUri` to the actual output.
The function has no command-line subcommand: it reads storage and auth settings
from the documented environment. `AWS_REGION` and role credentials come from
Lambda. Do not set the local DynamoDB endpoint override in AWS.

An operator who has reviewed the resources, IAM policy, retention and costs can
deploy with:

```bash
sam deploy --guided --template-file deploy/remote-stateless/lambda/template.yaml \
  --capabilities CAPABILITY_IAM \
  --parameter-overrides TokenSecretArn=YOUR_EXISTING_SECRET_ARN StorageNamespace=team SharedOrg=false EnableWriteTools=1
```

`EnableWriteTools` accepts `0` or `1` and defaults to `0`. The override above
explicitly enables authenticated writes through `KIMETSU_MCP_ENABLE_WRITE_TOOLS`;
use `EnableWriteTools=0` for read-only deployments. Without this opt-in, a new
empty deployment can be healthy while unable to record its first memory.

The `BaseUrl` output is the MCP base URL. Requests still need a bearer token and
use `/mcp/<repo>`. API Gateway provides HTTPS; the Lambda binary adapts invocation
events to the same router without opening a TCP listener. All instances share
DynamoDB checkpoints and rate counters. Reserved concurrency, 2 GiB memory,
2 GiB `/tmp` and a 28-second timeout are example starting values. Large checkpoints
can exceed the gateway latency budget well below the storage size limit: measure
representative requests before relying on this configuration. Lean Lambda avoids
embedding/reranker downloads; adding semantic models requires a separate package,
native-library, cold-start and storage budget review.

Retained tables and orphan chunks need an operator maintenance policy. Stop
writers before an orphan sweep; keep chunks referenced by current manifests.
Never add TTL to durable data. No live AWS deployment is performed by CI, and
local emulator tests do not validate AWS IAM or service quotas.

References: [SAM Function properties](https://docs.aws.amazon.com/serverless-application-model/latest/developerguide/sam-resource-function.html),
[Lambda custom runtime bootstrap](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-custom.html),
[Cargo Lambda build](https://www.cargo-lambda.info/commands/build.html),
[cross-compilation caveats](https://www.cargo-lambda.info/guide/cross-compiling.html).
