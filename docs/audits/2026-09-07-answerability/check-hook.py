"""Isolated CLI hook regression; no embedding model or API calls."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = str(Path(sys.argv[1]).resolve())
env = dict(os.environ, KIMETSU_USER_BRAIN="0", KIMETSU_BRAIN_EMBEDDER="noop",
           KIMETSU_EMBED_DAEMON="0", KIMETSU_TIER="free")
for enabled, memory, expected, query in [
    (True, "The staging listener password is managed in configuration. The staging listener binds port 6319.", None, "What password does the staging listener require?"),
    (False, "The staging listener password is managed in configuration. The staging listener binds port 6319.", "managed", "What password does the staging listener require?"),
    (True, "Staging listener connection settings - the listener port is 6319.", "6319", "What port does the staging listener use?"),
]:
    with tempfile.TemporaryDirectory(prefix="answerability-hook-") as folder:
        def run(*args, input=None):
            result = subprocess.run([binary, *args], cwd=folder, env=env, input=input,
                                    text=True, encoding="utf-8", capture_output=True)
            assert result.returncode == 0, result.stderr
            return result.stdout
        subprocess.run(["git", "init", "--quiet"], cwd=folder, check=True)
        run("init")
        for key, value in [("broker.explicit_fact_guard", str(enabled).lower()),
                           ("broker.warm_start", "false"), ("broker.min_lexical_coverage", "0.0"),
                           ("broker.abstain_min_score", "0.0")]:
            run("config", "set", key, value)
        assert run("config", "get", "broker.explicit_fact_guard").strip() == str(enabled).lower()
        run("brain", "memory", "add", "--scope", "project", "--kind", "fact", memory)
        result = run("brain", "context-hook", input=json.dumps({"session_id": "guard-check",
                      "prompt": query}))
        assert (not result.strip()) if expected is None else (expected in result), result
        print(f"PASS: guard={enabled}, expected={expected!r}")
