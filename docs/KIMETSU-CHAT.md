# Kimetsu Chat Setup

`kimetsu chat` is the user-facing Rust harness. It does not require Harbor;
Harbor is only the Terminal-Bench adapter.

## Token Setup

Kimetsu resolves `CLAUDE_CODE_OAUTH_TOKEN` in this order:

1. The current process environment.
2. The workspace `.env` file passed by `--workspace`.

For a local checkout:

```pwsh
Copy-Item .env.example .env
notepad .env
cargo run -p kimetsu-cli -- chat --workspace .
```

The `.env` entry should be:

```dotenv
CLAUDE_CODE_OAUTH_TOKEN=<your-token>
```

`.env` is git-ignored; `.env.example` is the committed template. Each user
keeps their own token locally.

## Common Commands

```pwsh
cargo run -p kimetsu-cli -- chat --workspace .
cargo run -p kimetsu-cli -- chat --workspace . --project .
cargo run -p kimetsu-cli -- chat --workspace . --max-cost-usd 1
```

Set `KIMETSU_MODEL` in the process environment to change the default model.
Set `KIMETSU_CLAUDE_PERSISTENT=0` to disable persistent Claude subprocesses.
