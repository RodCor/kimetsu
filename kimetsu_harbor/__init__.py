"""Kimetsu's Harbor / Terminal-Bench adapter package.

This package lives outside the Rust workspace because Harbor is a Python
harness â€” it imports an `agent-import-path` and instantiates the class
inside its own runtime. The adapter wraps `kimetsu-harbor-agent`
(MP-7a) and bridges Harbor's `environment.exec()` to kimetsu's JSON-RPC
`tool.exec` request frames.

See `kimetsu_harbor/README.md` for setup + usage.
"""

__version__ = "0.2.0"
