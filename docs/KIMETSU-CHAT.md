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

## Terminal UI

When stdout is an interactive terminal, `kimetsu chat` renders a lightweight
ANSI interface with the chibi dragon banner, session status, readable assistant
blocks, and turn/cache summaries. Redirected output and tests stay plain.

Use these flags when needed:

```pwsh
cargo run -p kimetsu-cli -- chat --workspace . --plain
cargo run -p kimetsu-cli -- chat --workspace . --no-logo
```

`NO_COLOR=1` or `KIMETSU_PLAIN=1` also disables the rich terminal renderer.
Inside chat, `/clear` drops the in-session transcript and redraws the banner;
`/logo` shows the dragon banner again.

## Skills

Kimetsu can load Agent Skills, Codex skills, and Claude Code compatible skills.
A skill is the full folder bundle. `SKILL.md` is only the required entrypoint:
it provides `name` and `description` frontmatter plus activation
instructions, while bundled files stay available for on-demand use.

```text
.codex/skills/refactor/
|-- SKILL.md
|-- scripts/
|   `-- check.ps1
|-- references/
|   `-- guide.md
|-- assets/
|   `-- schema.json
`-- templates/
    `-- example.txt
```

Kimetsu discovers the skill folder, injects the activated `SKILL.md`
instructions, and includes a compact resource inventory in the model context.
Bundled scripts, references, assets, templates, and other files are read or run
only when the active task calls for them. This follows the Agent Skills
progressive-disclosure model documented at <https://agentskills.io/home>.

Load skills at startup by name or path:

```pwsh
cargo run -p kimetsu-cli -- chat --workspace . --skill refactor
cargo run -p kimetsu-cli -- chat --workspace . --skill-dir C:\Users\you\.codex\skills --skill refactor
cargo run -p kimetsu-cli -- chat --workspace . --skill .\.claude\skills\frontend-design
```

Inside chat:

```text
/skills list
/skills use refactor
/skills loaded
/skills clear
```

By default Kimetsu scans workspace `.codex/skills`, `.claude/skills`, and
`.kimetsu/skills`. Use `--no-workspace-skills` to disable those default roots.
Use `--list-skills` to print discovered skills, their entrypoints, and bundled
resource counts before exiting.
