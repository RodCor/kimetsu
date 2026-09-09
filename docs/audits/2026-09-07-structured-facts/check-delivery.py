"""Isolated real CLI/MCP probes, using Noop embeddings and no model calls."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = str(Path(sys.argv[1]).resolve())
env = dict(os.environ, KIMETSU_USER_BRAIN="0", KIMETSU_BRAIN_EMBEDDER="noop",
           KIMETSU_EMBED_DAEMON="0", KIMETSU_TIER="free",
           KIMETSU_MCP_ENABLE_WRITE_TOOLS="1",
           KIMETSU_DETECT_CONFLICTS="0", KIMETSU_RESOLVE_CONFLICTS="0")
cases = [
    ("agent-mcp-record", ["Orchid staging gateway port is 7319."],
     "What are the Orchid staging gateway port and timeout?", "partial", "timeout", 4),
    ("partial", ["Orchid staging gateway port is 7319."],
     "What are the Orchid staging gateway port and timeout?", "partial", "timeout", 4),
    ("conflict-cap", ["Orchid staging gateway port is 7319. Operators record the listener settings.",
                      "Orchid staging gateway port is 8420. The deployment checklist records a different current value."],
     "What is the Orchid staging gateway port?", "conflicting", "port", 1),
    ("equivalent-units", ["Orchid staging gateway timeout is 30 seconds.",
                          "Orchid staging gateway timeout is 30000 ms."],
     "What is the Orchid staging gateway timeout?", "supported", None, 4),
    ("conflict-intermediate-budget", ["Orchid staging gateway port is 7319.",
                                     "Orchid staging gateway port is 8420. " + "deployment-checklist " * 900],
     "What is the Orchid staging gateway port?", "conflicting", "port", 4),
    ("wrong-environment", ["Orchid production gateway port is 8420."],
     "What is the Orchid staging gateway port?", "missing", "port", 4),
]
for name, memories, query, status, attribute, cap in cases:
    with tempfile.TemporaryDirectory(prefix="structured-facts-") as folder:
        def run(*args, input=None):
            result = subprocess.run([binary, *args], cwd=folder, env=env, input=input,
                                    text=True, encoding="utf-8", capture_output=True, timeout=90)
            assert result.returncode == 0, result.stderr
            return result.stdout
        subprocess.run(["git", "init", "--quiet"], cwd=folder, check=True)
        run("init")
        for key, value in [("broker.explicit_fact_guard", "true"), ("broker.warm_start", "false"),
                           ("embedder.reranker", "off"),
                           ("broker.min_lexical_coverage", "0.0"), ("broker.abstain_min_score", "0.0")]:
            run("config", "set", key, value)
        for memory in memories:
            if name != "agent-mcp-record":
                run("brain", "memory", "add", "--scope", "project", "--kind", "fact", memory)
        messages = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "fact-probe", "version": "1"}}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "kimetsu_brain_context", "arguments": {"query": query, "budget_tokens": 6000, "include_ambient": False, "max_capsules": cap}}},
        ]
        if name == "agent-mcp-record":
            messages.insert(2, {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "kimetsu_brain_record", "arguments": {"lesson": memories[0], "tags": ["network", "gateway"]}}})
        output = run("mcp", "serve", "--workspace", folder, input="".join(json.dumps(m)+"\n" for m in messages))
        response = next(json.loads(line) for line in output.splitlines() if json.loads(line).get("id") == 2)
        assert "error" not in response, response
        payload = json.loads(next(c["text"] for c in response["result"]["content"] if c["type"] == "text"))
        assert payload["answerability"]["status"] == status, payload
        if attribute:
            field = "conflicting" if status == "conflicting" else "missing"
            assert attribute in payload["answerability"][field], payload
        assert len(payload["capsules"]) <= cap, payload
        hook = run("brain", "context-hook", "--max-capsules", str(cap), "--min-score", "0.0",
                   input=json.dumps({"session_id": name, "prompt": query}))
        if status in ("partial", "conflicting"):
            expected = f"conflicting values for {attribute}" if status == "conflicting" else f"no supported value for {attribute}"
            assert expected in hook, hook
        if status == "missing":
            assert not hook.strip(), hook
        print(json.dumps({"probe": name, "answerability": payload["answerability"], "hook": hook.strip()}))
