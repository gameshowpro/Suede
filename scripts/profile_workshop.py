#!/usr/bin/env python3
"""
Profiles Suede's projection performance on the workshop machine across workloads.
Gathers CPU (slicer, sway, chromium), GPU utilization, frame rate, inter-display sync,
and pipeline latencies over multiple 10s sampling windows.
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request
import statistics

API = "http://localhost:9088/api/v1"

def api_get(path):
    url = f"{API}{path}"
    req = urllib.request.Request(url)
    with urllib.request.urlopen(req, timeout=10) as resp:
        return json.loads(resp.read().decode("utf-8"))

def api_post(path, data=None):
    url = f"{API}{path}"
    body = json.dumps(data).encode("utf-8") if data is not None else None
    headers = {"Content-Type": "application/json"} if data is not None else {}
    req = urllib.request.Request(url, data=body, headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=10) as resp:
        content = resp.read().decode("utf-8")
        return json.loads(content) if content else {}

def get_pids():
    # slicer
    try:
        slicer = subprocess.check_output(["pgrep", "-f", "suede slice"]).decode().split()
    except subprocess.CalledProcessError:
        slicer = []
    # sway
    try:
        sway = subprocess.check_output(["pgrep", "-x", "sway"]).decode().split()
    except subprocess.CalledProcessError:
        sway = []
    # chromium
    try:
        chromium = subprocess.check_output(["pgrep", "-f", "chromium"]).decode().split()
    except subprocess.CalledProcessError:
        chromium = []
    return {
        "slicer": slicer[0] if slicer else None,
        "sway": sway[0] if sway else None,
        "chromium": chromium,
    }

def sample_cpu(pids):
    cpu_stats = {"slicer": 0.0, "sway": 0.0, "chromium_total": 0.0}
    all_pids = []
    if pids["slicer"]:
        all_pids.append(pids["slicer"])
    if pids["sway"]:
        all_pids.append(pids["sway"])
    all_pids.extend(pids["chromium"])

    if not all_pids:
        return cpu_stats

    try:
        out = subprocess.check_output(
            ["ps", "-o", "pid=,%cpu=", "-p", ",".join(all_pids)]
        ).decode().strip()
        for line in out.splitlines():
            parts = line.split()
            if len(parts) >= 2:
                pid, cpu = parts[0], float(parts[1])
                if pid == pids["slicer"]:
                    cpu_stats["slicer"] = cpu
                elif pid == pids["sway"]:
                    cpu_stats["sway"] = cpu
                elif pid in pids["chromium"]:
                    cpu_stats["chromium_total"] += cpu
    except Exception as e:
        pass
    return cpu_stats

def sample_gpu():
    try:
        out = subprocess.check_output(
            ["nvidia-smi", "--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"]
        ).decode().strip()
        return float(out.splitlines()[0])
    except Exception:
        return 0.0

def profile_workload(app_id, duration_intervals=3, warmup_seconds=20):
    print(f"\n--- Activating {app_id} (warmup {warmup_seconds}s) ---", flush=True)
    api_post(f"/apps/{app_id}/activate")
    time.sleep(warmup_seconds)

    # Wait for the next fresh interval to start
    initial_stats = api_get("/projection/stats")
    last_measured_at = (initial_stats.get("lastInterval") or {}).get("measuredAt", 0)

    intervals = []
    cpu_samples = []
    gpu_samples = []

    print(f"Sampling {duration_intervals} intervals for {app_id}...", flush=True)
    pids = get_pids()
    print(f"Detected PIDs: slicer={pids['slicer']}, sway={pids['sway']}, chromium={len(pids['chromium'])} procs", flush=True)

    for i in range(duration_intervals):
        interval_cpu = []
        interval_gpu = []
        start_time = time.time()

        # Wait until a new interval is published in projection/stats
        while True:
            # Sample CPU and GPU every 1s
            interval_cpu.append(sample_cpu(pids))
            interval_gpu.append(sample_gpu())
            time.sleep(1.0)

            stats = api_get("/projection/stats")
            cur_interval = stats.get("lastInterval")
            if cur_interval:
                cur_measured = cur_interval.get("measuredAt", 0)
                if cur_measured > last_measured_at and (time.time() - start_time) >= 8.0:
                    last_measured_at = cur_measured
                    intervals.append(cur_interval)
                    cpu_samples.append(interval_cpu)
                    gpu_samples.append(interval_gpu)
                    print(f"  Interval {i+1}/{duration_intervals}: "
                          f"fps={cur_interval.get('presentedFps', 0):.2f}, "
                          f"canvas_fps={cur_interval.get('canvasFps', 0):.2f}, "
                          f"gpu_time={cur_interval.get('perFrameMs', {}).get('gpu', 0):.2f}ms, "
                          f"avg_gpu_util={statistics.mean(interval_gpu):.1f}%, "
                          f"avg_slicer_cpu={statistics.mean([c['slicer'] for c in interval_cpu]):.1f}%",
                          flush=True)
                    break

    return {
        "appId": app_id,
        "intervals": intervals,
        "cpuSamples": cpu_samples,
        "gpuSamples": gpu_samples,
    }

def summarize_run(run_data):
    intervals = run_data["intervals"]
    cpu_samples = run_data["cpuSamples"]
    gpu_samples = run_data["gpuSamples"]

    all_gpu_utils = [g for sublist in gpu_samples for g in sublist]
    all_slicer_cpus = [c["slicer"] for sublist in cpu_samples for c in sublist]
    all_sway_cpus = [c["sway"] for sublist in cpu_samples for c in sublist]
    all_chromium_cpus = [c["chromium_total"] for sublist in cpu_samples for c in sublist]

    fps_list = [inv.get("presentedFps", 0) for inv in intervals]
    canvas_fps_list = [inv.get("canvasFps", 0) for inv in intervals]
    gpu_time_list = [inv.get("perFrameMs", {}).get("gpu", 0) for inv in intervals]
    blending_time_list = [inv.get("perFrameMs", {}).get("blending", 0) for inv in intervals]
    waiting_time_list = [inv.get("perFrameMs", {}).get("waiting", 0) for inv in intervals]
    straddles_list = [inv.get("straddles", 0) for inv in intervals]
    gate_holds_list = [inv.get("gateHolds", 0) for inv in intervals]

    # Output phases
    outputs_phase = {}
    outputs_zero_copy = {}
    for inv in intervals:
        for out in inv.get("outputs", []):
            name = out.get("name")
            if name not in outputs_phase:
                outputs_phase[name] = []
                outputs_zero_copy[name] = []
            outputs_phase[name].append(out.get("phaseMs", 0.0))
            pres = out.get("presented", 0)
            zc = out.get("zeroCopyPresented", 0)
            outputs_zero_copy[name].append(100.0 * zc / pres if pres > 0 else 100.0)

    # Compute inter-display phase spread (max - min phase among all outputs per interval)
    phase_spreads = []
    for inv in intervals:
        phases = [out.get("phaseMs", 0.0) for out in inv.get("outputs", [])]
        if phases:
            phase_spreads.append(max(phases) - min(phases))

    return {
        "presentedFpsMean": statistics.mean(fps_list) if fps_list else 0,
        "canvasFpsMean": statistics.mean(canvas_fps_list) if canvas_fps_list else 0,
        "gpuUtilMean": statistics.mean(all_gpu_utils) if all_gpu_utils else 0,
        "gpuUtilMax": max(all_gpu_utils) if all_gpu_utils else 0,
        "slicerCpuMean": statistics.mean(all_slicer_cpus) if all_slicer_cpus else 0,
        "swayCpuMean": statistics.mean(all_sway_cpus) if all_sway_cpus else 0,
        "chromiumCpuMean": statistics.mean(all_chromium_cpus) if all_chromium_cpus else 0,
        "gpuTimeMsMean": statistics.mean(gpu_time_list) if gpu_time_list else 0,
        "blendingTimeMsMean": statistics.mean(blending_time_list) if blending_time_list else 0,
        "waitingTimeMsMean": statistics.mean(waiting_time_list) if waiting_time_list else 0,
        "straddlesTotal": sum(straddles_list),
        "gateHoldsTotal": sum(gate_holds_list),
        "phaseSpreadMsMean": statistics.mean(phase_spreads) if phase_spreads else 0,
        "phaseSpreadMsMax": max(phase_spreads) if phase_spreads else 0,
        "outputPhases": {k: statistics.mean(v) for k, v in outputs_phase.items()},
        "outputZeroCopyPct": {k: statistics.mean(v) for k, v in outputs_zero_copy.items()},
    }

def main():
    parser = argparse.ArgumentParser(description="Profile workshop workloads")
    parser.add_argument("--arm", required=True, help="Label for this test arm (e.g. baseline, simple, warp)")
    parser.add_argument("--out", required=True, help="Output JSON path")
    parser.add_argument("--intervals", type=int, default=3, help="Number of 10s intervals to sample")
    parser.add_argument("--warmup", type=int, default=20, help="Warmup seconds before sampling")
    args = parser.parse_args()

    sys_info = api_get("/system")
    status_info = api_get("/status")
    config_info = api_get("/config")

    print(f"Profiling arm: {args.arm}")
    print(f"System: suede={sys_info.get('suedeVersion')} ({sys_info.get('buildId')}), sway={sys_info.get('swayVersion')}")
    print(f"Current activeApp: {status_info.get('activeApp')}")

    results = {
        "arm": args.arm,
        "timestamp": time.time(),
        "system": sys_info,
        "config": config_info,
        "workloads": {}
    }

    # 1. Low complexity workload: sync-test
    sync_data = profile_workload("sync-test", duration_intervals=args.intervals, warmup_seconds=args.warmup)
    results["workloads"]["sync-test"] = {
        "raw": sync_data,
        "summary": summarize_run(sync_data)
    }

    # 2. High complexity workload: seascape
    seascape_data = profile_workload("seascape", duration_intervals=args.intervals, warmup_seconds=args.warmup)
    results["workloads"]["seascape"] = {
        "raw": seascape_data,
        "summary": summarize_run(seascape_data)
    }

    with open(args.out, "w") as f:
        json.dump(results, f, indent=2)

    print(f"\nSaved profiling results to {args.out}")

    # Print summary table
    print("\n" + "=" * 80)
    print(f"SUMMARY FOR ARM: {args.arm} ({sys_info.get('buildId')})")
    print("=" * 80)
    for w_name in ["sync-test", "seascape"]:
        sm = results["workloads"][w_name]["summary"]
        print(f"\nWorkload: {w_name}")
        print(f"  Frame Rate: presented={sm['presentedFpsMean']:.2f} fps, canvas={sm['canvasFpsMean']:.2f} fps")
        print(f"  GPU Util:   {sm['gpuUtilMean']:.1f}% (max: {sm['gpuUtilMax']:.1f}%)")
        print(f"  CPU Util:   slicer={sm['slicerCpuMean']:.1f}%, sway={sm['swayCpuMean']:.1f}%, chromium={sm['chromiumCpuMean']:.1f}%")
        print(f"  Timings:    gpu={sm['gpuTimeMsMean']:.2f}ms, blending={sm['blendingTimeMsMean']:.2f}ms, waiting={sm['waitingTimeMsMean']:.2f}ms")
        print(f"  Sync:       mean phase spread={sm['phaseSpreadMsMean']:.3f}ms (max {sm['phaseSpreadMsMax']:.3f}ms), straddles={sm['straddlesTotal']}, gate holds={sm['gateHoldsTotal']}")
        phases_str = ", ".join([f"{k}: {v:.2f}ms" for k, v in sm["outputPhases"].items()])
        zc_str = ", ".join([f"{k}: {v:.1f}%" for k, v in sm["outputZeroCopyPct"].items()])
        print(f"  Phases:     {phases_str}")
        print(f"  ZeroCopy:   {zc_str}")
    print("=" * 80)

if __name__ == "__main__":
    main()
