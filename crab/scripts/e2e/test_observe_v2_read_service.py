import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from observe_v2_read_service import Observation, metrics_values, resource_gates


class ObservationTests(unittest.TestCase):
    def test_replica_exit_retains_failure_report_and_stops_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "server"
            binary.write_bytes(b"fixture")
            receipt = root / "build.json"
            receipt.write_text(json.dumps({"server_binary":str(binary),
                "server_binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest()}))
            profile = {"schema":1, "build_receipt":str(receipt),
                "replicas":[{"pid":12345, "metrics_url":"https://localhost:1234/metrics", "maximum_rss_bytes":1024}],
                "tls":{"ca":"ca", "certificate":"cert", "key":"key"},
                "provider_url":"http://127.0.0.1:1234/provider", "maximum_late_rss_ratio":1.1,
                "sample_interval_seconds":10, "quota_enforcement":"test"}
            with patch("observe_v2_read_service.ssl.create_default_context"), \
                 patch("observe_v2_read_service.process_identity", return_value="fixture-start"), \
                 patch("observe_v2_read_service.Observation.fetch", return_value=b"{}"), \
                 patch("observe_v2_read_service.resource_sample", return_value={"rss_bytes":512, "ps_cpu_percent":0}), \
                 patch("observe_v2_read_service.metrics_values", return_value={"crab_git_cache_bytes":0}):
                observation = Observation(profile, root / "observations")
                observation.start()
                with patch.object(observation, "fetch", side_effect=ConnectionRefusedError), \
                     patch("observe_v2_read_service.process_identity", side_effect=ProcessLookupError):
                    report = observation.finish()
            self.assertEqual(report["status"], "failed")
            self.assertIn("sample:final_sample:ConnectionRefusedError", report["failures"])
            self.assertIn("sample:final_provider:ConnectionRefusedError", report["failures"])
            self.assertIn("replica-0:identity:ProcessLookupError", report["failures"])
            self.assertEqual(json.loads((root / "observations/report.json").read_text()), report)
            self.assertFalse(observation.thread.is_alive())

    def test_metrics_keep_map_occupancy_distinct_from_admitted_owners(self):
        values = metrics_values("""# TYPE crab_git_cache_bytes gauge
crab_git_cache_bytes{class="blame"} 512
crab_git_resident_read_bytes 4096
unrelated 10
""")
        self.assertEqual(values, {'crab_git_cache_bytes{class="blame"}':512, 'crab_git_resident_read_bytes':4096})
        with self.assertRaises(ValueError):
            metrics_values('crab_git_resident_read_bytes NaN')

    def test_rss_and_retained_owner_violations_are_reported_independently(self):
        profile = {"replicas":[{"maximum_rss_bytes":1024}], "maximum_late_rss_ratio":1.1}
        def sample(rss=512, used=512, maximum=1024):
            return {"elapsed_seconds":0, "replicas":[{"rss_bytes":rss, "metrics":{
                "crab_git_resident_read_bytes":used, "crab_git_resident_read_capacity_bytes":maximum,
            }}]}
        self.assertEqual(resource_gates([sample()] * 20, profile), [])
        self.assertEqual(resource_gates([sample()] * 10 + [sample(2048)] * 10, profile),
                         ["replica-0:rss_envelope"])
        self.assertEqual(resource_gates([sample(used=2048)] * 20, profile),
                         ["replica-0:resident_admission"])
        soak = [sample() for _ in range(16)] + [sample(600) for _ in range(4)]
        for index, row in enumerate(soak):
            row["elapsed_seconds"] = index * 60
        self.assertEqual(resource_gates(soak, profile),
                         ["replica-0:rss_growth"])
        for index, row in enumerate(soak):
            row["elapsed_seconds"] = index
        self.assertEqual(resource_gates(soak, profile), [])


if __name__ == "__main__":
    unittest.main()
