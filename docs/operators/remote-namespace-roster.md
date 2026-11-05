# Binding a Remote namespace to a bus roster

ADR 0001 open question 4. The bind is manual. This page is the operator record.
It does not enroll namespaces, and it does not make a bearer token a pack signer.

Kimetsu Remote and the memory bus are different trust domains.

| | Remote namespace | Bus roster |
| --- | --- | --- |
| What it names | A storage prefix for repo and optional org checkpoints | Endpoints, the mounts each endpoint may serve or request, and the pack-signing keys it may publish |
| Credential | Bearer token (`--token`, `--tokens-file`, `KIMETSU_REMOTE_TOKEN`) | Ed25519. The operator pins the roster admin public key. Pack signatures use the keys listed under `signers` |
| What a credential proves | The caller may use that namespace's HTTP MCP surface, within repo and global-token rules | The roster document and, separately, a pack's bytes and scopes |

A bearer token is not an ed25519 pack signer. Presenting one does not trust a signer, does not publish a roster entry, and does not import a pack. A roster signature does not mint a Remote token. Defaulting one org key to do both is intentionally unset.

## What the operator writes down

Keep the association next to the deployment notes, not in the checkpoint and not in application code. One namespace, one roster, one pinned admin key:

```toml
namespace = "team"
roster_id = "7f3c0a1b9e24"
roster_admin_public_key = "<64 lowercase hex chars of the roster admin>"
```

`namespace` is the Remote `--storage-namespace` (`KIMETSU_REMOTE_STORAGE_NAMESPACE`). `roster_id` and `roster_admin_public_key` are the values the operator already installed for the bus (`kimetsu_roster` 1). The file is data. Nothing in `kimetsu-remote` reads it to dial a peer or to skip signature checks.

To change the bind, edit the record and install the roster on the machines that should speak the bus. Remote does not watch this file.

## What stays separate

- Remote does not dial the bus. A pack enters Remote only through `import_pack` on a server that holds the destination grant, or through the existing checkpoint publish.
- Checkpoint limits are unchanged: 64 MiB compressed, 256 MiB decoded, 2,000,000 rows, format 1 locked to the binary's brain schema. The per-request SQLite file is still a disposable projection. See [remote-storage.md](../remote-storage.md).
- `@kimetsu-ai/sdk` is the HTTP client of that MCP surface. It does not open `brain.db`, does not verify pack signatures, and does not join a roster.
- The in-process Rust entry for `context`, `record`, `cite`, `search`, `supersede`, `export_pack`, `import_pack`, and `request_pack` is the `kimetsu-sdk` crate. `request_pack` forwards to `kimetsu-bus`. Remote calls the store functions from `crates/kimetsu-remote/src/store.rs` and from the MCP record and cite tools. Remote does not call `request_pack`. That crate is not the npm package.
