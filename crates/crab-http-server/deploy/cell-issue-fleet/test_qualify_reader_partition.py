"""Require authority evidence before accepting a closed replica listener."""

import io
import unittest
import urllib.error
from unittest.mock import patch

from qualify_reader_partition import unavailable


class PartitionEvidenceTests(unittest.TestCase):
    def error(self, status, body):
        return urllib.error.HTTPError("http://fixture/issue", status, "failure", {}, io.BytesIO(body))

    def test_live_reader_requires_typed_unavailability(self):
        error = self.error(503, b'{"error":{"code":"replica_unavailable"}}')
        with patch("urllib.request.urlopen", side_effect=error):
            self.assertEqual(unavailable("http://fixture/issue")["status"], 503)
        self.assertTrue(error.closed)

    def test_empty_gateway_error_requires_expired_session(self):
        for expired in (False, True):
            error = self.error(502, b"")
            with self.subTest(expired=expired), patch("urllib.request.urlopen", side_effect=error):
                if expired:
                    self.assertIsNone(unavailable("http://fixture/issue", withdrawn=True)["body"])
                else:
                    with self.assertRaises(ValueError):
                        unavailable("http://fixture/issue")
                self.assertTrue(error.closed)

    def test_expiry_does_not_accept_malformed_or_unrelated_errors(self):
        for status, body in ((503, b"null"), (502, b'{}'), (503, b'{"error":{"code":"unrelated"}}')):
            error = self.error(status, body)
            with self.subTest(status=status, body=body), patch("urllib.request.urlopen", side_effect=error):
                with self.assertRaises(RuntimeError):
                    unavailable("http://fixture/issue", withdrawn=True)
                self.assertTrue(error.closed)


if __name__ == "__main__":
    unittest.main()
