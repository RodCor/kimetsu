---
name: kimetsu-brain
description: Use when Pi tasks benefit from prior session knowledge, durable lessons, memory corrections, or feedback on helpful memories.
---
Kimetsu is a persistent memory sidecar accessed through the `kimetsu` CLI.
Run commands from the relevant project directory. If the binary is unavailable,
note the absence and continue normally.

## Use the context already provided

The Pi extension retrieves context before each task. Read that injection first.
When it covers the current question, proceed without repeating the same lookup.
Use `kimetsu brain context "<specific question>"` when no useful context was
injected, the task changes, or a missing detail or correction needs fresh evidence.
An empty result is a reason to inspect the repository, not repeat the same query.

Memory is evidence from earlier work. Check conflicts against current files and
the user's instructions. Respect project, environment, and version boundaries;
partial or conflicting evidence does not justify filling gaps with assumptions.

## Record and correct durable lessons

After verifying a reusable lesson, record it with
`kimetsu brain memory add --scope project --kind <kind> "<lesson>"`.
Choose `fact`, `preference`, `convention`, `command`, or `failure_pattern`.
Include the subject and any environment/version limits in the text.

For a correction to an existing claim, update its actual memory ID instead of
adding a contradictory duplicate:

```sh
kimetsu brain memory edit <memory-id> --text "Production gateway port is 4000. Development gateway port remains 3000."
```

Preserve still-valid parts of the claim. Different environments or historical
versions can both be valid; a production correction does not retire development
guidance. When the entire memory is obsolete or false, use
`kimetsu brain memory invalidate <memory-id> --reason "<verified reason>"`.

Find actual IDs with `kimetsu brain context "<specific question>" --json` or
`kimetsu brain memory list --json`; match the text and scope before editing,
invalidating, or citing. For a `memory:<id>` expansion handle, use only `<id>`.
Never invent an ID or treat a file capsule as a memory.
The edit/invalidate commands above operate on the current workspace brain.
Listings may also include portable user-brain memories; those commands cannot
correct them. For a user-brain claim, report the correction and this limitation
instead of claiming the portable memory was updated.

## Credit explicit usefulness

When a particular memory materially helped, record that reliance:

```sh
kimetsu brain cite --memory-id <memory-id> --query "<task it helped with>" --note "<how it helped>"
```

Cite only memories actually used. Being injected, or having tests pass, is not
enough to credit a memory. A citation records usefulness, not proof of truth;
correct wrong claims using the commands above. Do not cite unused memories or
repeat credit for the same use.

`kimetsu brain status` reports initialization, accepted memories, and pending
proposals. Automatic session saving is separate from these deliberate actions;
model-based lesson distillation requires a configured distiller.
