import unittest

from observe_v2_read_service import metrics_values, resource_gates


class ObservationTests(unittest.TestCase):
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
