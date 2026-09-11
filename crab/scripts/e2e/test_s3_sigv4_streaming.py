#!/usr/bin/env python3
"""Tests for the streaming S3 SigV4 qualification client."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("s3_sigv4_streaming.py")
SPEC = importlib.util.spec_from_file_location("s3_sigv4_streaming", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
STREAMING = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = STREAMING
SPEC.loader.exec_module(STREAMING)


class SigV4StreamingTests(unittest.TestCase):
    def test_aws_streaming_example_seed_signature(self) -> None:
        headers = {
            "content-encoding": "aws-chunked",
            "content-length": "66824",
            "host": "s3.amazonaws.com",
            "x-amz-content-sha256": STREAMING.PAYLOAD_MODE,
            "x-amz-date": "20130524T000000Z",
            "x-amz-decoded-content-length": "66560",
            "x-amz-storage-class": "REDUCED_REDUNDANCY",
        }
        canonical = STREAMING._canonical_request(
            "PUT",
            "/examplebucket/chunkObject.txt",
            headers,
            sorted(headers),
        )
        signature, _, _ = STREAMING._seed_signature(
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "20130524T000000Z",
            "us-east-1",
            "s3",
            canonical,
        )

        self.assertEqual(
            signature,
            "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9",
        )

    def test_encoded_length_matches_body(self) -> None:
        chunks = [b"a" * (64 * 1024), b"b" * 1024]
        body = STREAMING._encoded_body(
            chunks,
            b"0" * 32,
            "20130524T000000Z",
            "20130524/us-east-1/s3/aws4_request",
            "0" * 64,
            False,
        )

        self.assertEqual(len(body), STREAMING._encoded_length(chunks))

    def test_tampering_changes_only_first_chunk_signature(self) -> None:
        arguments = (
            [b"payload"],
            b"0" * 32,
            "20130524T000000Z",
            "20130524/us-east-1/s3/aws4_request",
            "0" * 64,
        )
        valid = STREAMING._encoded_body(*arguments, False)
        tampered = STREAMING._encoded_body(*arguments, True)

        self.assertNotEqual(valid, tampered)
        self.assertEqual(valid.split(b"\r\n", 1)[1], tampered.split(b"\r\n", 1)[1])


if __name__ == "__main__":
    unittest.main()
