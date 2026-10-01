#!/usr/bin/env python3
"""Collect display-pipeline profiling data from the Suede API or offline inputs."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

API = "http://localhost:9088/api/v1"
PROC = Path("/proc")
STATS_READ_CHUNK = 8 * 1024 * 1024
MAX_STATS_LINE = 1024 * 1024


def parse_proc_stat(text):
    """Parse Linux proc stat safely when comm contains spaces or parentheses."""
    close = text.rfind(")")
    if close < 0:
        raise ValueError("malformed /proc stat: missing comm terminator")
    pid_text = text[:text.find(" (")].strip()
    fields = text[close + 1:].split()  # field 3 onward
    if len(fields) <= 19:
        raise ValueError("malformed /proc stat: too few fields")
    return {"pid": int(pid_text), "comm": text[text.find(" (") + 2:close],
            "state": fields[0], "ppid": int(fields[1]),
            "utime": int(fields[11]), "stime": int(fields[12]),
            "starttime": int(fields[19])}


def cpu_delta(previous, current, ticks_per_second=None):
    """Return CPU percent of one core, rejecting PID reuse and counter resets."""
    if (previous["pid"], previous["starttime"]) != (current["pid"], current["starttime"]):
        return None
    ticks = current["utime"] + current["stime"] - previous["utime"] - previous["stime"]
    elapsed = current["sample_monotonic"] - previous["sample_monotonic"]
    if ticks < 0 or elapsed <= 0:
        return None
    ticks_per_second = ticks_per_second or os.sysconf("SC_CLK_TCK")
    return ticks * 100.0 / ticks_per_second / elapsed


def aggregate_role_cpu(pids, processes, previous):
    """Return a total only when every current process has a valid paired sample."""
    if not pids:
        return {"total": None, "partialTotal": 0.0, "validProcesses": 0, "expectedProcesses": 0}
    total, valid = 0.0, 0
    for pid in pids:
        current = processes[pid]
        old = previous.get(str(pid))
        delta = cpu_delta(old, current) if old else None
        if delta is not None:
            total += delta
            valid += 1
    return {"total": total if valid == len(pids) else None,
            "partialTotal": total, "validProcesses": valid, "expectedProcesses": len(pids)}


def read_processes():
    found = {}
    for path in PROC.iterdir():
        if not path.name.isdigit():
            continue
        try:
            stat = parse_proc_stat((path / "stat").read_text())
            stat["argv"] = [part.decode(errors="replace") for part in (path / "cmdline").read_bytes().split(b"\0") if part]
            stat["cmdline"] = " ".join(stat["argv"])
            try:
                stat["exe"] = os.readlink(path / "exe")
            except OSError:
                stat["exe"] = ""
            stat["sample_monotonic"] = time.monotonic()
            found[stat["pid"]] = stat
        except (OSError, ValueError, ProcessLookupError):
            continue
    return found


def classify(processes):
    """Assign processes to roles, including descendants of browser roots."""
    self_pid = os.getpid()
    browser_roots = set()
    roles = {"sway": set(), "daemon": set(), "slicer": set(), "browser": set()}
    for pid, proc in processes.items():
        if pid == self_pid:
            continue
        comm = proc["comm"].lower()
        argv = proc.get("argv", proc.get("cmdline", "").split())
        exe = os.path.basename(proc.get("exe", ""))
        if exe.endswith(" (deleted)"):
            exe = exe[:-10]
        exe = exe.lower()
        if exe == "sway" or (not exe and comm == "sway"):
            roles["sway"].add(pid)
        if exe == "suede" or (not exe and comm == "suede"):
            args = [os.path.basename(arg).lower() for arg in argv[1:]]
            if "slice" in args:
                roles["slicer"].add(pid)
            else:
                roles["daemon"].add(pid)
        if exe in {"chromium", "chromium-browser", "chrome", "firefox"} or (not exe and comm in {"chromium", "chromium-b", "chrome", "firefox"}):
            browser_roots.add(pid)
    browser_pids = set(browser_roots)
    changed = True
    while changed:
        changed = False
        for pid, proc in processes.items():
            if pid != self_pid and proc["ppid"] in browser_pids and pid not in browser_pids:
                browser_pids.add(pid)
                changed = True
    roles["browser"] = browser_pids
    return roles


def get_json(path, timeout=4):
    req = urllib.request.Request(API + path, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as response:
        return json.loads(response.read().decode("utf-8"))


def load_metadata_snapshot(path):
    """Load exported API metadata without treating it as live health data."""
    try:
        value = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read metadata snapshot {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError("metadata snapshot must be a JSON object")
    return sanitize(value)


def is_projection_stats(value):
    return isinstance(value, dict) and all(
        key in value for key in ("measuredAt", "intervalSeconds", "perFrameMs", "outputs")
    )


def stats_observation(stats, last_stats_json, last_measured):
    """Return serialized state and whether it adds a new measured interval."""
    interval = stats.get("lastInterval") or {}
    measured = interval.get("measuredAt")
    serialized = json.dumps(stats, sort_keys=True, separators=(",", ":"))
    changed = serialized != last_stats_json
    fresh = changed and measured is not None and measured != last_measured
    return serialized, measured, fresh, changed


class StatsJsonlReader:
    """Incrementally read completed stats lines, retaining only the newest interval."""

    def __init__(self, path):
        self.path = Path(path)
        self.offset = 0
        self.partial = b""
        self.line_number = 0
        self.latest = None
        self.checkpoint = b""

    def read_latest(self):
        try:
            size = self.path.stat().st_size
            if size < self.offset:
                self.offset = 0
                self.partial = b""
                self.line_number = 0
                self.latest = None
                self.checkpoint = b""
            with self.path.open("rb") as stream:
                if self.checkpoint:
                    check_at = self.offset - len(self.checkpoint)
                    stream.seek(check_at)
                    if stream.read(len(self.checkpoint)) != self.checkpoint:
                        self.offset = 0
                        self.partial = b""
                        self.line_number = 0
                        self.latest = None
                        self.checkpoint = b""
                stream.seek(self.offset)
                chunk = stream.read(STATS_READ_CHUNK)
                self.offset = stream.tell()
        except OSError as error:
            raise ValueError(f"cannot read slicer stats file {self.path}: {error}") from error

        data = self.partial + chunk
        self.checkpoint = data[-64:]
        lines = data.split(b"\n")
        self.partial = lines.pop()
        if len(self.partial) > MAX_STATS_LINE:
            raise ValueError(f"slicer stats line exceeds {MAX_STATS_LINE} bytes")
        for raw in lines:
            self.line_number += 1
            if not raw.strip():
                continue
            if len(raw) > MAX_STATS_LINE:
                raise ValueError(f"slicer stats line {self.line_number} exceeds {MAX_STATS_LINE} bytes")
            try:
                value = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                raise ValueError(f"malformed complete JSON line {self.line_number} in {self.path}: {error}") from error
            if is_projection_stats(value):
                self.latest = value
        return self.latest


def sanitize(value):
    if isinstance(value, dict):
        return {k: ("[redacted]" if re.search(r"password|secret|token|credential|authorization", k, re.I) else sanitize(v)) for k, v in value.items()}
    if isinstance(value, list):
        return [sanitize(v) for v in value]
    return value


def digest(value):
    raw = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(raw).hexdigest()


def binary_metadata(processes):
    result = {}
    hashes = {}

    def staged_suede_path(proc):
        argv = proc.get("argv", [])
        candidate = Path(argv[0]) if argv else None
        if (proc.get("comm", "").lower() == "suede" and candidate and candidate.is_absolute()
                and candidate.name == "suede" and candidate.is_file()):
            return str(candidate)
        return None

    for role, pids in classify(processes).items():
        for pid in sorted(pids):
            proc = processes[pid]
            caveat = None
            try:
                exe = proc.get("exe") or None
                hash_path = f"/proc/{pid}/exe" if exe else None
                if not hash_path:
                    exe = staged_suede_path(proc)
                    hash_path = exe
                    caveat = "hashed argv[0] path; executable link unavailable" if exe else None
                try:
                    if hash_path and exe not in hashes:
                        with open(hash_path, "rb") as binary:
                            hasher = hashlib.sha256()
                            for chunk in iter(lambda: binary.read(1024 * 1024), b""):
                                hasher.update(chunk)
                            hashes[exe] = hasher.hexdigest()
                except OSError:
                    fallback = staged_suede_path(proc)
                    if not fallback or fallback == hash_path:
                        raise
                    exe, hash_path = fallback, fallback
                    caveat = "hashed argv[0] path; executable link unavailable"
                    if exe not in hashes:
                        with open(hash_path, "rb") as binary:
                            hasher = hashlib.sha256()
                            for chunk in iter(lambda: binary.read(1024 * 1024), b""):
                                hasher.update(chunk)
                            hashes[exe] = hasher.hexdigest()
                sha = hashes.get(exe) if exe else None
            except OSError:
                exe, sha = None, None
            result[f"{role}:{pid}:{proc['starttime']}"] = {"exe": exe, "sha256": sha,
                                                                  "hashPathCaveat": caveat}
    return result


def parse_gpu_field(raw):
    """Parse one nvidia-smi CSV field, or None if it reports unavailable.

    nvidia-smi's own N/A spelling is driver- and field-dependent: some fields
    (and drivers) say `N/A`, others wrap it as `[N/A]` or `[Not Supported]`.
    Stripping brackets before comparing means one field the driver declines
    to report (`power.draw` on an RTX A1000, for example) is treated as
    "unavailable", not a parse failure — see the per-field handling in
    `gpu_sample`, which keeps the rest of the row's numbers either way.
    """
    text = raw.strip()
    normalized = text.strip("[]").strip().lower()
    if normalized in {"n/a", "not supported"}:
        return None
    return float(text)


def gpu_sample():
    fields = ["utilization.gpu", "power.draw", "clocks.gr", "temperature.gpu"]
    try:
        result = subprocess.run(["nvidia-smi", "--query-gpu=" + ",".join(fields), "--format=csv,noheader,nounits"],
                                capture_output=True, text=True, timeout=3, check=True)
        row = result.stdout.strip().splitlines()[0].split(",")
        if len(row) != len(fields):
            raise ValueError(f"expected {len(fields)} gpu fields, got {len(row)}: {row!r}")
        values = {}
        for key, raw in zip(fields, row):
            # A single field that is unexpectedly unparsable (not just the
            # known N/A spellings) still should not cost the whole sample:
            # every other field the driver did report stays usable.
            try:
                values[key] = parse_gpu_field(raw)
            except ValueError:
                values[key] = None
        values["reason"] = None
        return values
    except (OSError, subprocess.SubprocessError, IndexError, ValueError) as error:
        values = {key: None for key in fields}
        values["reason"] = str(error)
        return values


def collect(args):
    stats_file = getattr(args, "stats_file", None)
    metadata_file = getattr(args, "metadata_file", None)
    if stats_file and not metadata_file:
        raise ValueError("--stats-file requires --metadata-file")
    data = {"label": args.label, "startedUtc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "requestedDurationSeconds": args.duration, "sampleIntervalSeconds": args.interval,
            "metadata": {}, "samples": [], "statsChanges": [], "processEvents": [], "errors": []}
    processes = read_processes()
    offline_inputs = bool(stats_file)
    if metadata_file:
        snapshot = load_metadata_snapshot(metadata_file)
        data["metadata"].update(snapshot)
        data["metadataProvenance"] = {
            "source": "saved-metadata-snapshot",
            "snapshotFile": str(metadata_file),
            "health": "snapshot only; not live health",
            "harness": "isolated slice session; daemon expected stopped" if offline_inputs else "live daemon session",
        }
        for name in ("system", "status", "config", "outputs", "checks"):
            if name in snapshot:
                data["metadata"][name + "Sha256"] = digest(snapshot[name])
    else:
        data["metadataProvenance"] = {
            "source": "daemon-api",
            "health": "live API metadata",
            "harness": "live daemon session",
        }
        for name, path in (("system", "/system"), ("status", "/status"), ("config", "/config"), ("outputs", "/outputs"), ("checks", "/system/checks")):
            try:
                value = sanitize(get_json(path))
                data["metadata"][name] = value
                data["metadata"][name + "Sha256"] = digest(value)
            except Exception as error:
                data["errors"].append({"at": "metadata:" + name, "error": str(error)})
    data["metadata"]["binaries"] = binary_metadata(processes)
    data["metadata"]["kernel"] = platform.uname()._asdict()
    try:
        data["metadata"]["gpuIdentity"] = subprocess.run(
            ["nvidia-smi", "--query-gpu=name,driver_version", "--format=csv,noheader"],
            capture_output=True, text=True, timeout=3, check=True).stdout.strip()
    except (OSError, subprocess.SubprocessError) as error:
        data["metadata"]["gpuIdentity"] = {"reason": str(error)}
    roles_at_start = classify(processes)
    sway = next((processes[pid] for pid in roles_at_start["sway"]), None)
    if sway:
        try:
            env = Path(f"/proc/{sway['pid']}/environ").read_bytes().split(b"\0")
            data["metadata"]["swayWlrEnvironment"] = {
                item.split(b"=", 1)[0].decode(): item.split(b"=", 1)[1].decode(errors="replace")
                for item in env if item.startswith(b"WLR_") and b"=" in item}
        except OSError as error:
            data["metadata"]["swayWlrEnvironment"] = {"reason": str(error)}
    bootstrap_path = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "suede/suede.toml"
    try:
        bootstrap = bootstrap_path.read_text()
        data["metadata"]["bootstrap"] = bootstrap
        data["metadata"]["bootstrapSha256"] = hashlib.sha256(bootstrap.encode()).hexdigest()
    except OSError as error:
        data["metadata"]["bootstrap"] = {"path": str(bootstrap_path), "reason": str(error)}
    if offline_inputs:
        data["metadata"]["rendererStartupLine"] = None
        data["metadata"]["rendererStartupLineSource"] = "not collected; daemon expected stopped"
    else:
        try:
            journal = subprocess.run(["journalctl", "--user", "-u", "suede", "-n", "80", "--no-pager", "-o", "cat"],
                                     capture_output=True, text=True, timeout=3)
            lines = [line for line in journal.stdout.splitlines() if "slicer: renderer" in line.lower()]
            data["metadata"]["rendererStartupLine"] = lines[-1] if lines else None
            data["metadata"]["rendererStartupLineSource"] = "suede user journal"
        except (OSError, subprocess.SubprocessError) as error:
            data["metadata"]["rendererStartupLine"] = {"reason": str(error)}
            data["metadata"]["rendererStartupLineSource"] = "suede user journal"
    start = time.monotonic()
    offline_stats = StatsJsonlReader(stats_file) if stats_file else None
    data["statsProvenance"] = {
        "source": "slicer-stdout-jsonl" if offline_stats else "suede-projection-api",
        "file": str(stats_file) if offline_stats else None,
        "processLiveness": "not inferred from stats file" if offline_stats else "reported by API",
    }

    def read_stats():
        if offline_stats:
            # This wrapper intentionally has no `running` field: the file says
            # nothing about whether the slicer process is still alive.
            return {"lastInterval": offline_stats.read_latest()}
        return get_json("/projection/stats")

    try:
        initial_stats = read_stats()
        last_stats_json, last_measured, _, _ = stats_observation(initial_stats, None, None)
        data["statsChanges"].append({"wallTime": time.time(), "measuredAt": last_measured,
                                     "freshInterval": False, "stats": initial_stats,
                                     "note": "initial snapshot; not counted as a fresh interval"})
    except Exception as error:
        last_measured, last_stats_json = None, None
        data["errors"].append({"at": "projection/stats:initial", "wallTime": time.time(), "error": str(error)})
    previous = {}
    seen_roles = {}
    expected_roles = ["sway", "slicer", "browser"] if offline_inputs else ["sway", "daemon", "slicer", "browser"]
    data["expectedRoles"] = expected_roles
    interrupted = False

    def save():
        output = Path(args.output)
        output.parent.mkdir(parents=True, exist_ok=True)
        temp = output.with_name(output.name + f".tmp-{os.getpid()}")
        with open(temp, "w", encoding="utf-8") as stream:
            os.chmod(temp, 0o600)
            json.dump(data, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, output)

    try:
        while time.monotonic() - start < args.duration:
            sample_start = time.monotonic()
            processes = read_processes()
            roles = classify(processes)
            cpu = {}
            for role, pids in roles.items():
                current_ids = []
                for pid in sorted(pids):
                    proc = processes[pid]
                    identity = {k: proc[k] for k in ("pid", "starttime", "comm")}
                    current_ids.append(identity)
                cpu[role] = aggregate_role_cpu(pids, processes, previous)
                cpu[role + "Processes"] = current_ids
                role_identity = [(p["pid"], p["starttime"]) for p in current_ids]
                if role in seen_roles and seen_roles[role] != role_identity:
                    data["processEvents"].append({"wallTime": time.time(), "role": role,
                                                   "event": "restart_or_membership_change",
                                                   "previous": seen_roles[role], "current": role_identity})
                seen_roles[role] = role_identity
            sample = {"wallTime": time.time(), "monotonic": sample_start, "cpuPercentOneCore": cpu,
                      "gpu": gpu_sample(), "processes": {"missingRoles": [r for r in expected_roles if not roles.get(r)]}}
            for role, pids in roles.items():
                for pid in pids:
                    previous[str(pid)] = processes[pid]
            for old_pid in list(previous):
                if int(old_pid) not in processes:
                    del previous[old_pid]
            try:
                stats = read_stats()
                serialized, measured, fresh, changed = stats_observation(stats, last_stats_json, last_measured)
                if changed:
                    data["statsChanges"].append({"wallTime": time.time(), "measuredAt": measured,
                                                 "freshInterval": fresh, "stats": stats})
                    if fresh:
                        last_measured = measured
                if last_stats_json is None:
                    # The initial response is retained even if it has no interval yet.
                    data["statsChanges"][-1]["note"] = "initial snapshot; no prior change observed"
                last_stats_json = serialized
            except Exception as error:
                data["errors"].append({"at": "projection/stats", "wallTime": time.time(), "error": str(error)})
            data["samples"].append(sample)
            save()
            time.sleep(max(0, args.interval - (time.monotonic() - sample_start)))
    except KeyboardInterrupt:
        interrupted = True
    finally:
        data["endedUtc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        data["elapsedSeconds"] = time.monotonic() - start
        data["interrupted"] = interrupted
        save()
    return 130 if interrupted else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, help="Private JSON result path")
    parser.add_argument("--label", required=True)
    parser.add_argument("--duration", type=float, default=60)
    parser.add_argument("--interval", type=float, default=1)
    parser.add_argument("--stats-file", help="Read completed slicer ProjectionStats JSON lines instead of the live API (requires --metadata-file)")
    parser.add_argument("--metadata-file", help="Use an exported system/config/outputs JSON snapshot; health is snapshot data, not live")
    args = parser.parse_args()
    import math
    if not math.isfinite(args.duration) or not math.isfinite(args.interval) or args.duration <= 0 or args.interval <= 0:
        parser.error("--duration and --interval must be finite positive numbers")
    if args.stats_file and not args.metadata_file:
        parser.error("--stats-file requires --metadata-file")
    def terminate_as_interrupt(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, terminate_as_interrupt)
    try:
        return collect(args)
    except Exception as error:
        print(f"profile-display: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
