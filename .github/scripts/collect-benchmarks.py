#!/usr/bin/env python3
"""Collect Criterion means and confidence intervals without depending on console output."""

import argparse
import hashlib
import json
import math
import os
import platform
import subprocess
from datetime import datetime, timezone
from pathlib import Path


def collect(root, criterion, commit, profile=None):
    benchmarks = []
    source_hashes = {}
    for estimates_path in sorted(criterion.glob("**/new/estimates.json")):
        metadata = json.loads(estimates_path.with_name("benchmark.json").read_text())
        mean = json.loads(estimates_path.read_text())["mean"]
        interval = mean["confidence_interval"]
        sample = json.loads(estimates_path.with_name("sample.json").read_text())
        count = len(sample["times"])
        if count < 10 or count != len(sample["iters"]):
            raise ValueError(f"Invalid sample count for {metadata['full_id']}")
        values = [interval["lower_bound"], mean["point_estimate"], interval["upper_bound"]]
        if not all(math.isfinite(value) and value > 0 for value in values):
            raise ValueError(f"Invalid estimate for {metadata['full_id']}")
        if not values[0] <= values[1] <= values[2] or interval["confidence_level"] != 0.95:
            raise ValueError(f"Invalid confidence interval for {metadata['full_id']}")
        suite = metadata["group_id"].split("/", 1)[0]
        if suite not in source_hashes:
            source = root / "benches" / f"{suite}.rs"
            sources = [source] if source.exists() else sorted((root / "benches").rglob("*.rs"))
            sources += sorted((root / "benches" / "shared").rglob("*.rs"))
            digest = hashlib.sha256()
            for source in sources:
                digest.update(source.relative_to(root).as_posix().encode())
                digest.update(b"\0")
                digest.update(source.read_bytes().replace(b"\r\n", b"\n"))
            source_hashes[suite] = digest.hexdigest()
        benchmarks.append(dict(id=metadata["full_id"], source_hash=source_hashes[suite], samples=count, low_ns=values[0], mean_ns=values[1], high_ns=values[2]))
    if not benchmarks or len({row["id"] for row in benchmarks}) != len(benchmarks):
        raise ValueError("Missing or duplicate Criterion results")
    digest = hashlib.sha256()
    for source in sorted((root / "benches").rglob("*.rs")):
        digest.update(source.relative_to(root).as_posix().encode())
        digest.update(b"\0")
        digest.update(source.read_bytes().replace(b"\r\n", b"\n"))
    cpu = platform.processor()
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        cpu = next((line.split(":", 1)[1].strip() for line in cpuinfo.read_text().splitlines() if line.startswith("model name")), cpu)
    physical_cores = None
    try:
        topology = subprocess.check_output(["lscpu", "-p=SOCKET,CORE"], text=True)
        physical_cores = len({line for line in topology.splitlines() if line and not line.startswith("#")})
    except (OSError, subprocess.SubprocessError):
        pass
    memory_bytes = None
    meminfo = Path("/proc/meminfo")
    if meminfo.exists():
        memory_bytes = next((int(line.split()[1]) * 1024 for line in meminfo.read_text().splitlines() if line.startswith("MemTotal:")), None)
    try:
        os_name = platform.freedesktop_os_release().get("PRETTY_NAME", platform.system())
    except OSError:
        os_name = platform.system()
    return dict(
        commit=commit,
        measured_at=datetime.now(timezone.utc).isoformat(),
        suite_hash=digest.hexdigest(),
        toolchain=subprocess.check_output(["rustc", "--version"], text=True).strip(),
        platform=platform.platform(),
        cpu=cpu,
        physical_cores=physical_cores,
        logical_threads=os.cpu_count(),
        memory_bytes=memory_bytes,
        os=os_name,
        profile=profile or "release; all features; actual samples recorded per case; 95% confidence intervals",
        benchmarks=sorted(benchmarks, key=lambda row: row["id"]),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--criterion", type=Path, required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--profile")
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(collect(args.root, args.criterion, args.commit, args.profile), indent=2) + "\n")


if __name__ == "__main__":
    main()
