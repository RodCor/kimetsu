import io
import json
import math
import unittest
from unittest.mock import patch

import evaluate


class DecisionTests(unittest.TestCase):
    def test_official_request_shapes(self):
        openai = evaluate.request_body("openai", "query and memory")
        self.assertEqual(openai["model"], "gpt-6-luna")
        self.assertEqual(openai["input"], "query and memory")
        self.assertEqual(openai["questions"][0]["type"], "predicate")
        self.assertEqual(openai["questions"][0]["name"], "relevance")
        for provider, model in [("jev", "jev-latest"), ("perplexity", "pplx-decider-v1.1-27b")]:
            body = evaluate.request_body(provider, "query and memory")
            self.assertEqual(body["model"], model)
            self.assertEqual(body["state"], "query and memory")
            self.assertEqual(body["questions"]["relevance"]["type"], "noul")

    def test_response_contracts(self):
        self.assertEqual(evaluate.probability("openai", {"answers": [{"name": "relevance", "type": "predicate", "probability": 0.8}]}), 0.8)
        for provider in ["jev", "perplexity"]:
            self.assertEqual(evaluate.probability(provider, {"answers": {"relevance": {"type": "noul", "noul": 0.7}}}), 0.7)

    def test_invalid_responses_do_not_become_decisions(self):
        for value in [None, True, "0.8", -0.1, 1.1, math.nan, math.inf]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                evaluate.probability("openai", {"answers": [{"name": "relevance", "type": "predicate", "probability": value}]})
        for body in [{}, {"answers": []}, {"answers": [{"name": "other", "type": "predicate", "probability": 0.9}]}, {"answers": [{"name": "relevance", "type": "choice", "probability": 0.9}]}]:
            with self.subTest(body=body), self.assertRaises(ValueError):
                evaluate.probability("openai", body)

    def test_default_dry_run_never_reads_credentials_or_sends_requests(self):
        with patch.object(evaluate, "send", side_effect=AssertionError("network")), patch.object(evaluate.os, "getenv", side_effect=AssertionError("credentials")), patch("sys.stdin", io.StringIO("synthetic evidence")), patch("sys.stdout", new_callable=io.StringIO) as out:
            self.assertEqual(evaluate.main(["--provider", "jev"]), 0)
            self.assertEqual(json.loads(out.getvalue())["mode"], "dry-run")

    def test_live_requires_existing_key(self):
        with patch.dict(evaluate.os.environ, {}, clear=True), patch("sys.stdin", io.StringIO("synthetic evidence")), patch("sys.stderr", new_callable=io.StringIO), patch.object(evaluate, "send", side_effect=AssertionError("network")):
            self.assertEqual(evaluate.main(["--provider", "openai", "--live"]), 1)

    def test_oversized_input_rejected_without_request(self):
        with patch("sys.stdin", io.StringIO("x" * 65537)), patch("sys.stderr", new_callable=io.StringIO), patch.object(evaluate, "send", side_effect=AssertionError("network")):
            self.assertEqual(evaluate.main(["--provider", "jev"]), 1)

    def test_malformed_key_rejected_without_exposing_credential(self):
        secret = "synthetic-review-secret\ninvalid"
        with patch.dict(evaluate.os.environ, {"OPENAI_API_KEY": secret}, clear=True), patch("sys.stdin", io.StringIO("synthetic evidence")), patch("sys.stderr", new_callable=io.StringIO) as err, patch.object(evaluate.urllib.request, "build_opener") as opener:
            self.assertEqual(evaluate.main(["--provider", "openai", "--live"]), 1)
            opener.assert_not_called()
            self.assertNotIn("synthetic-review-secret", err.getvalue())

    def test_transport_value_error_is_sanitized(self):
        with patch.object(evaluate.urllib.request, "build_opener") as opener:
            opener.return_value.open.side_effect = ValueError("synthetic-transport-secret")
            with self.assertRaisesRegex(ValueError, "^Provider request failed;"):
                evaluate.send("openai", {}, "synthetic-key")


if __name__ == "__main__":
    unittest.main()
