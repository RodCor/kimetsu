"""Failure and output-contract checks for the two-physical-host harness.

Stub tests do not build Rust. Set F3_TWO_HOST_SMOKE=1 to compile the debug
example, refuse bad invocations, and run the loopback self-test. That
self-test is not a §7 measurement.
"""

import os
from pathlib import Path
import stat
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "f3-hotpath-two-hosts.sh"
SECRET = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
PENDING = (
    "two physical LAN hosts: still pending. "
    "This run is not direct LAN QUIC between two LAN machines."
)
GOOD_FETCH = textwrap.dedent(
    f"""\
    topology: local self-test on one host, not two physical LAN hosts
    bind: 127.0.0.1:1
    peer: 127.0.0.2:2
    pack: 394 gzip bytes, 8 memories, relay disabled
    {PENDING}
    """
)


def stub_source() -> str:
    return textwrap.dedent(
        f"""\
        #!/usr/bin/env python3
        import os
        import pathlib
        import sys

        args = sys.argv[1:]
        role = None
        state = None
        index = 0
        while index < len(args):
            if args[index] == "--role" and index + 1 < len(args):
                role = args[index + 1]
                index += 2
            elif args[index] == "--state" and index + 1 < len(args):
                state = args[index + 1]
                index += 2
            else:
                index += 1
        log = os.environ.get("STUB_LOG")
        if log:
            with open(log, "a", encoding="utf-8") as handle:
                handle.write(" ".join(args) + "\\n")
        if role == "client-init":
            directory = pathlib.Path(state)
            directory.mkdir(parents=True, exist_ok=True)
            secret = directory / "client.secret"
            secret.write_text("{SECRET}\\n", encoding="utf-8")
            secret.chmod(0o600)
            print("role: client-init")
            print("client_public: abcdef")
            print("note: this secret stays on this machine. Do not copy it to the server.")
        elif role == "serve-public":
            directory = pathlib.Path(state)
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "handoff.json").write_text('{{"kind":"public"}}\\n', encoding="utf-8")
        elif role == "fetch-public":
            sys.stdout.write(os.environ.get("FIXTURE_FETCH", ""))
        else:
            sys.stderr.write("stub missing role\\n")
            sys.exit(2)
        """
    )


class HarnessScript(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.stub = Path(self.tmp.name) / "stub.py"
        self.stub.write_text(stub_source(), encoding="utf-8")
        self.stub.chmod(self.stub.stat().st_mode | stat.S_IEXEC)
        self.log = Path(self.tmp.name) / "args.log"
        self.env = os.environ.copy()
        self.env["F3_HOTPATH_BIN"] = str(self.stub)
        self.env["STUB_LOG"] = str(self.log)
        self.env["FIXTURE_FETCH"] = GOOD_FETCH

    def run_script(self, args):
        return subprocess.run(
            ["sh", str(SCRIPT), *args],
            cwd=self.tmp.name,
            env=self.env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_usage_and_unknown_command(self):
        usage = self.run_script([])
        self.assertNotEqual(usage.returncode, 0)
        self.assertIn("client-init", usage.stdout)
        unknown = self.run_script(["nope"])
        self.assertNotEqual(unknown.returncode, 0)
        self.assertIn("unknown command", unknown.stderr)
        self.assertFalse(self.log.exists())

    def test_serve_rejects_loopback_veth_and_does_not_call_the_binary(self):
        state = str(Path(self.tmp.name) / "state")
        loopback = self.run_script(
            ["serve", "--bind", "127.0.0.1", "--client-public", "abc", "--state", state]
        )
        self.assertNotEqual(loopback.returncode, 0)
        self.assertIn("refusing loopback address 127.0.0.1", loopback.stderr)
        veth = self.run_script(
            ["serve", "--bind", "10.200.0.1", "--client-public", "abc", "--state", state]
        )
        self.assertNotEqual(veth.returncode, 0)
        self.assertIn("refusing veth harness address 10.200.0.1", veth.stderr)
        peer = self.run_script(
            [
                "fetch",
                "--bind", "192.168.1.20",
                "--peer", "10.200.0.2",
                "--state", state,
                "--handoff", str(Path(self.tmp.name) / "missing.json"),
            ]
        )
        self.assertNotEqual(peer.returncode, 0)
        self.assertIn("refusing veth harness address 10.200.0.2", peer.stderr)
        self.assertFalse(self.log.exists())

    def test_fetch_rejects_identical_addresses_and_a_missing_handoff(self):
        state = str(Path(self.tmp.name) / "state")
        handoff = Path(self.tmp.name) / "handoff.json"
        handoff.write_text("{}\n", encoding="utf-8")
        same = self.run_script(
            [
                "fetch",
                "--bind", "192.168.1.10",
                "--peer", "192.168.1.10",
                "--state", state,
                "--handoff", str(handoff),
            ]
        )
        self.assertNotEqual(same.returncode, 0)
        self.assertIn("refusing identical bind and peer", same.stderr)
        missing = self.run_script(
            [
                "fetch",
                "--bind", "192.168.1.20",
                "--peer", "192.168.1.10",
                "--state", state,
                "--handoff", str(Path(self.tmp.name) / "nope.json"),
            ]
        )
        self.assertNotEqual(missing.returncode, 0)
        self.assertIn("is missing", missing.stderr)
        self.assertFalse(self.log.exists())

    def test_confirm_is_forwarded_only_when_asked(self):
        state = str(Path(self.tmp.name) / "state")
        plain = self.run_script(
            ["serve", "--bind", "192.168.1.10", "--client-public", "abc", "--state", state]
        )
        self.assertEqual(plain.returncode, 0, plain.stderr)
        confirmed = self.run_script(
            [
                "serve",
                "--bind", "192.168.1.11",
                "--client-public", "abc",
                "--state", state,
                "--confirm-two-physical-hosts",
            ]
        )
        self.assertEqual(confirmed.returncode, 0, confirmed.stderr)
        text = self.log.read_text(encoding="utf-8")
        lines = text.strip().splitlines()
        self.assertNotIn("--confirm-two-physical-hosts", lines[0])
        self.assertIn("--confirm-two-physical-hosts", lines[1])
        self.assertNotIn("--self-test", text)
        self.assertIn("--role serve-public", lines[0])

    def test_client_init_does_not_print_the_secret(self):
        state = str(Path(self.tmp.name) / "client")
        result = self.run_script(["client-init", "--state", state])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("client_public: abcdef", result.stdout)
        self.assertNotIn(SECRET, result.stdout)
        self.assertNotIn(SECRET, result.stderr)
        self.assertTrue((Path(state) / "client.secret").is_file())

    def test_self_test_guard_rejects_a_lan_label_and_accepts_pending(self):
        self.env["FIXTURE_FETCH"] = "topology: two physical LAN hosts\n" + PENDING + "\n"
        bad = self.run_script(["self-test"])
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("claimed two physical LAN hosts", bad.stderr)
        self.env["FIXTURE_FETCH"] = GOOD_FETCH
        good = self.run_script(["self-test", "--quic-samples", "2"])
        self.assertEqual(good.returncode, 0, good.stderr)
        self.assertIn(PENDING, good.stdout)
        self.assertIn("must not be copied into the §7 row", good.stdout)
        self.assertNotIn("topology: two physical LAN hosts\n", good.stdout)
        logged = self.log.read_text(encoding="utf-8")
        self.assertNotIn("--confirm-two-physical-hosts", logged)
        self.assertIn("--self-test", logged)

    def test_self_test_refuses_confirm(self):
        result = self.run_script(["self-test", "--confirm-two-physical-hosts"])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing --confirm-two-physical-hosts on a self-test", result.stderr)
        self.assertFalse(self.log.exists())

    def test_cleanup_requires_the_marker_and_removes_a_marked_directory(self):
        unmarked = Path(self.tmp.name) / "plain"
        unmarked.mkdir()
        (unmarked / "keep").write_text("x", encoding="utf-8")
        refused = self.run_script(["cleanup", "--state", str(unmarked)])
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("missing kimetsu-f3-two-host-state marker", refused.stderr)
        self.assertTrue((unmarked / "keep").is_file())
        root = self.run_script(["cleanup", "--state", "/"])
        self.assertNotEqual(root.returncode, 0)
        self.assertIn("refusing to delete", root.stderr)
        marked = Path(self.tmp.name) / "marked"
        marked.mkdir()
        (marked / "kimetsu-f3-two-host-state").write_text(
            "kimetsu-f3-two-host-state\n", encoding="utf-8"
        )
        (marked / "client.secret").write_text(SECRET, encoding="utf-8")
        removed = self.run_script(["cleanup", "--state", str(marked)])
        self.assertEqual(removed.returncode, 0, removed.stderr)
        self.assertFalse(marked.exists())


@unittest.skipUnless(os.environ.get("F3_TWO_HOST_SMOKE") == "1", "set F3_TWO_HOST_SMOKE=1")
class LocalSmoke(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        env = os.environ.copy()
        env["F3_CARGO_PROFILE"] = "debug"
        env["KIMETSU_BRAIN_EMBEDDER"] = "off"
        env["KIMETSU_USER_BRAIN"] = "0"
        built = subprocess.run(
            ["cargo", "+stable", "build", "-p", "kimetsu-bus", "--example", "f3_hotpath"],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        if built.returncode != 0:
            raise AssertionError(built.stderr[-4000:])
        cls.bin = ROOT / "target" / "debug" / "examples" / "f3_hotpath"
        cls.env = env
        cls.env["F3_HOTPATH_BIN"] = str(cls.bin)

    def run_bin(self, args):
        return subprocess.run(
            [str(self.bin), *args],
            cwd=ROOT,
            env=self.env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_debug_binary_refuses_confirm_public_lan_and_a_secret_handoff(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary) / "client"
            init = self.run_bin(["--role", "client-init", "--state", str(state)])
            self.assertEqual(init.returncode, 0, init.stderr)
            secret = (state / "client.secret").read_text(encoding="utf-8").strip()
            self.assertNotIn(secret, init.stdout)
            self.assertEqual(stat.S_IMODE((state / "client.secret").stat().st_mode), 0o600)
            public = [
                line.split(": ", 1)[1]
                for line in init.stdout.splitlines()
                if line.startswith("client_public: ")
            ][0]
            confirm = self.run_bin(
                [
                    "--role", "serve-public",
                    "--confirm-two-physical-hosts",
                    "--bind", "192.0.2.10",
                    "--client-public", public,
                    "--state", str(Path(temporary) / "server"),
                ]
            )
            self.assertNotEqual(confirm.returncode, 0)
            self.assertIn("debug build", confirm.stderr)
            loopback = self.run_bin(
                [
                    "--role", "serve-public",
                    "--self-test",
                    "--bind", "192.0.2.10",
                    "--client-public", public,
                    "--state", str(Path(temporary) / "server"),
                ]
            )
            self.assertNotEqual(loopback.returncode, 0)
            self.assertIn("self-test only binds loopback", loopback.stderr)
            veth = self.run_bin(
                [
                    "--role", "serve-public",
                    "--self-test",
                    "--bind", "10.200.0.2",
                    "--client-public", public,
                    "--state", str(Path(temporary) / "server"),
                ]
            )
            self.assertNotEqual(veth.returncode, 0)
            self.assertIn("refusing veth harness address 10.200.0.2", veth.stderr)
            handoff = Path(temporary) / "handoff.json"
            handoff.write_text(
                '{"kind":"kimetsu-f3-two-host-handoff","client_secret_hex":"aa"}\n',
                encoding="utf-8",
            )
            smuggled = self.run_bin(
                [
                    "--role", "fetch-public",
                    "--self-test",
                    "--bind", "127.0.0.1",
                    "--peer", "127.0.0.2",
                    "--state", str(state),
                    "--handoff", str(handoff),
                ]
            )
            self.assertNotEqual(smuggled.returncode, 0)
            self.assertIn("private key", smuggled.stderr)

    def test_script_self_test_stays_pending(self):
        env = self.env.copy()
        env["F3_CARGO_PROFILE"] = "debug"
        result = subprocess.run(
            ["sh", str(SCRIPT), "self-test", "--quic-samples", "2"],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr[-4000:] + result.stdout[-2000:])
        self.assertIn(PENDING, result.stdout)
        self.assertIn("must not be copied into the §7 row", result.stdout)
        self.assertNotIn("topology: two physical LAN hosts\n", result.stdout)
        self.assertIn("relay disabled", result.stdout)
        self.assertIn("embeddings: off", result.stdout)
        self.assertIn("import and FTS rebuild: not included in the request_pack timer", result.stdout)


if __name__ == "__main__":
    unittest.main()
