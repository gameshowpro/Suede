"""Focused tests for profile-display's proc parsing and CPU deltas."""

import importlib.util
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

MODULE_PATH = Path(__file__).with_name("profile-display.py")
SPEC = importlib.util.spec_from_file_location("profile_display", MODULE_PATH)
profile_display = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(profile_display)


def proc_stat(pid, comm, utime, stime, starttime, ppid=1):
    fields = ["S", str(ppid)] + ["0"] * 18
    fields[11] = str(utime)
    fields[12] = str(stime)
    fields[19] = str(starttime)
    return f"{pid} ({comm}) " + " ".join(fields)


class ProcStatTests(unittest.TestCase):
    def test_parses_comm_with_spaces_and_parentheses(self):
        parsed = profile_display.parse_proc_stat(proc_stat(123, "browser (worker) thread", 40, 7, 9001, 55))
        self.assertEqual(parsed["pid"], 123)
        self.assertEqual(parsed["comm"], "browser (worker) thread")
        self.assertEqual(parsed["ppid"], 55)
        self.assertEqual(parsed["utime"], 40)
        self.assertEqual(parsed["stime"], 7)
        self.assertEqual(parsed["starttime"], 9001)

    def test_cpu_delta_is_percent_of_one_core(self):
        before = {"pid": 4, "starttime": 100, "utime": 20, "stime": 5, "sample_monotonic": 10.0}
        after = {"pid": 4, "starttime": 100, "utime": 30, "stime": 5, "sample_monotonic": 12.0}
        self.assertAlmostEqual(profile_display.cpu_delta(before, after, ticks_per_second=100), 5.0)

    def test_cpu_delta_rejects_reused_pid_and_counter_reset(self):
        before = {"pid": 4, "starttime": 100, "utime": 20, "stime": 5, "sample_monotonic": 10.0}
        reused = {"pid": 4, "starttime": 101, "utime": 400, "stime": 5, "sample_monotonic": 12.0}
        reset = {"pid": 4, "starttime": 100, "utime": 10, "stime": 5, "sample_monotonic": 12.0}
        self.assertIsNone(profile_display.cpu_delta(before, reused, ticks_per_second=100))
        self.assertIsNone(profile_display.cpu_delta(before, reset, ticks_per_second=100))

    def test_aggregate_is_null_when_any_process_is_unpaired(self):
        current = {
            1: {"pid": 1, "starttime": 100, "utime": 30, "stime": 5, "sample_monotonic": 12.0},
            2: {"pid": 2, "starttime": 200, "utime": 4, "stime": 1, "sample_monotonic": 12.0},
        }
        previous = {"1": {"pid": 1, "starttime": 100, "utime": 20, "stime": 5, "sample_monotonic": 10.0}}
        aggregate = profile_display.aggregate_role_cpu({1, 2}, current, previous)
        self.assertIsNone(aggregate["total"])
        self.assertAlmostEqual(aggregate["partialTotal"], 5.0)
        self.assertEqual(aggregate["validProcesses"], 1)


class ProcessClassificationTests(unittest.TestCase):
    def test_ignores_wrapper_command_text_and_separates_suede_roles(self):
        processes = {
            1001: {"pid": 1001, "ppid": 1000, "comm": "bash", "cmdline": "bash -c chromium --foo"},
            1002: {"pid": 1002, "ppid": 1001, "comm": "suede", "exe": "/usr/bin/suede", "argv": ["suede", "slice"]},
            1003: {"pid": 1003, "ppid": 1001, "comm": "suede", "exe": "/usr/bin/suede", "argv": ["suede", "run"]},
            1004: {"pid": 1004, "ppid": 1001, "comm": "chromium", "exe": "/usr/lib/chromium/chromium", "argv": ["chromium", "--type=renderer"]},
            1005: {"pid": 1005, "ppid": 1004, "comm": "renderer", "exe": "/usr/lib/chromium/chromium", "argv": ["chromium", "--type=renderer"]},
        }
        with patch.object(profile_display.os, "getpid", return_value=99999):
            roles = profile_display.classify(processes)
        self.assertEqual(roles["slicer"], {1002})
        self.assertEqual(roles["daemon"], {1003})
        self.assertEqual(roles["browser"], {1004, 1005})
        self.assertEqual(roles["sway"], set())


class GpuSampleTests(unittest.TestCase):
    def make_result(self, stdout):
        return type("CompletedProcess", (), {"stdout": stdout, "returncode": 0})()

    def test_parse_gpu_field_accepts_plain_and_bracketed_na_spellings(self):
        for spelling in ("N/A", "n/a", " N/A ", "[N/A]", "[n/a]", "[Not Supported]", "[NOT SUPPORTED]"):
            self.assertIsNone(profile_display.parse_gpu_field(spelling))

    def test_parse_gpu_field_parses_numbers(self):
        self.assertEqual(profile_display.parse_gpu_field(" 42 "), 42.0)
        self.assertEqual(profile_display.parse_gpu_field("0"), 0.0)

    def test_gpu_sample_keeps_other_fields_when_one_field_is_bracketed_na(self):
        # Observed on an RTX A1000: power.draw comes back as "[N/A]" while
        # the other fields are ordinary numbers. Before the fix this raised
        # inside the dict comprehension and discarded the whole sample,
        # including the perfectly valid utilization.gpu reading.
        stdout = "0, [N/A], 210, 43\n"
        with patch.object(profile_display.subprocess, "run", return_value=self.make_result(stdout)):
            sample = profile_display.gpu_sample()
        self.assertEqual(sample["utilization.gpu"], 0.0)
        self.assertIsNone(sample["power.draw"])
        self.assertEqual(sample["clocks.gr"], 210.0)
        self.assertEqual(sample["temperature.gpu"], 43.0)
        self.assertIsNone(sample["reason"])

    def test_gpu_sample_records_reason_and_nulls_all_fields_on_subprocess_failure(self):
        with patch.object(profile_display.subprocess, "run", side_effect=OSError("no such file")):
            sample = profile_display.gpu_sample()
        self.assertIsNone(sample["utilization.gpu"])
        self.assertIsNone(sample["power.draw"])
        self.assertIsNone(sample["clocks.gr"])
        self.assertIsNone(sample["temperature.gpu"])
        self.assertIn("no such file", sample["reason"])

    def test_gpu_sample_nulls_only_the_unparsable_field_on_unexpected_garbage(self):
        stdout = "0, garbage, 210, 43\n"
        with patch.object(profile_display.subprocess, "run", return_value=self.make_result(stdout)):
            sample = profile_display.gpu_sample()
        self.assertEqual(sample["utilization.gpu"], 0.0)
        self.assertIsNone(sample["power.draw"])
        self.assertEqual(sample["clocks.gr"], 210.0)
        self.assertEqual(sample["temperature.gpu"], 43.0)
        self.assertIsNone(sample["reason"])


class OfflineInputTests(unittest.TestCase):
    def stats(self, measured_at):
        return {"measuredAt": measured_at, "intervalSeconds": 10.0,
                "perFrameMs": {}, "outputs": []}

    def test_stats_reader_ignores_partial_tail_and_control_events(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "slicer.jsonl"
            first = self.stats(10)
            path.write_text(json.dumps({"event": "started"}) + "\n" + json.dumps(first) + "\n" + '{"measuredAt":', encoding="utf-8")
            reader = profile_display.StatsJsonlReader(path)
            self.assertEqual(reader.read_latest(), first)
            with path.open("a", encoding="utf-8") as stream:
                stream.write('20,"intervalSeconds":10,"perFrameMs":{},"outputs":[]}\n')
            self.assertEqual(reader.read_latest(), self.stats(20))

    def test_stats_reader_surfaces_malformed_complete_lines(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "slicer.jsonl"
            path.write_text('{"broken":}\n', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "malformed complete JSON line 1"):
                profile_display.StatsJsonlReader(path).read_latest()

    def test_stats_reader_resets_after_truncation(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "slicer.jsonl"
            path.write_text(json.dumps(self.stats(10)) + "\n", encoding="utf-8")
            reader = profile_display.StatsJsonlReader(path)
            self.assertEqual(reader.read_latest()["measuredAt"], 10)
            path.write_text(json.dumps(self.stats(11)) + "\n", encoding="utf-8")
            self.assertEqual(reader.read_latest()["measuredAt"], 11)

    def test_stats_changes_do_not_count_same_interval_as_fresh(self):
        wrapper = {"lastInterval": self.stats(10)}
        serialized, measured, fresh, changed = profile_display.stats_observation(wrapper, None, None)
        self.assertEqual(measured, 10)
        self.assertTrue(changed)
        self.assertTrue(fresh)
        same = {"lastInterval": {**self.stats(10), "diagnostic": "updated"}}
        _, measured, fresh, changed = profile_display.stats_observation(same, serialized, 10)
        self.assertEqual(measured, 10)
        self.assertTrue(changed)
        self.assertFalse(fresh)

    def test_metadata_snapshot_is_an_object_and_sanitized(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "metadata.json"
            path.write_text('{"system":{"hostname":"wall"},"config":{"apiToken":"private"}}', encoding="utf-8")
            snapshot = profile_display.load_metadata_snapshot(path)
            self.assertEqual(snapshot["system"]["hostname"], "wall")
            self.assertEqual(snapshot["config"]["apiToken"], "[redacted]")

    def test_offline_stats_requires_metadata_snapshot(self):
        args = type("Args", (), {"stats_file": "stats.jsonl", "metadata_file": None,
                                  "label": "test", "duration": 1, "interval": 1})()
        with self.assertRaisesRegex(ValueError, "--stats-file requires --metadata-file"):
            profile_display.collect(args)

    def test_binary_metadata_hashes_staged_suede_argv_when_exe_link_is_unavailable(self):
        with tempfile.TemporaryDirectory() as directory:
            staged = Path(directory) / "suede"
            staged.write_bytes(b"experimental binary")
            processes = {123: {"pid": 123, "starttime": 77, "comm": "suede", "exe": "",
                               "argv": [str(staged), "slice"]}}
            with patch.object(profile_display, "classify", return_value={"slicer": {123}}):
                metadata = profile_display.binary_metadata(processes)["slicer:123:77"]
            self.assertEqual(metadata["exe"], str(staged))
            self.assertEqual(metadata["sha256"], hashlib.sha256(staged.read_bytes()).hexdigest())
            self.assertEqual(metadata["hashPathCaveat"], "hashed argv[0] path; executable link unavailable")


if __name__ == "__main__":
    unittest.main()
