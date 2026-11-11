#!/usr/bin/env bash
set -euo pipefail

root_dir="$PWD"
head_sha="$(git rev-parse HEAD)"
base_sha="${BENCH_BASE_SHA:-}"
output_dir="${BENCH_OUTPUT_DIR:-${RUNNER_TEMP:-/tmp}/benchmark-results}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root_dir/target}"
mkdir -p "$output_dir"
collector="$root_dir/.github/scripts/collect-benchmarks.py"

measure() {
  local source_dir="$1" label="$2" sha="$3"
  local estimates_dir
  estimates_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/pbt-criterion-$label.XXXXXX")"
  (
    cd "$source_dir"
    mapfile -t suites < <(cargo metadata --locked --all-features --no-deps --format-version 1 | python3 -c '
import json, sys
data = json.load(sys.stdin)
package = next(package for package in data["packages"] if package["id"] in data["workspace_default_members"])
print("\n".join(target["name"] for target in package["targets"] if "bench" in target["kind"]))
')
    if [[ "${#suites[@]}" == 0 ]]; then echo "No benchmark targets found" >&2; exit 1; fi
    arguments=(--locked --all-features)
    for suite in "${suites[@]}"; do arguments+=(--bench "$suite"); done
    CRITERION_HOME="$estimates_dir" cargo bench "${arguments[@]}" -- \
      --sample-size 10 --warm-up-time 0.5 --measurement-time 1 --noplot
    python3 "$collector" --root "$source_dir" --criterion "$estimates_dir" \
      --commit "$sha" --output "$output_dir/$label.json"
  )
}

if [[ "$base_sha" =~ ^[0-9a-f]{40}$ && ! "$base_sha" =~ ^0+$ ]]; then
  base_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/pbt-benchmark-base.XXXXXX")"
  git worktree add --detach "$base_dir" "$base_sha"
  measure "$base_dir" base "$base_sha"
else
  printf 'null\n' > "$output_dir/base.json"
fi
measure "$root_dir" head "$head_sha"

python3 - "$output_dir" <<'PY'
import json
import sys
from pathlib import Path

directory = Path(sys.argv[1])
result = {"schema": 1, "base": json.loads((directory / "base.json").read_text()),
          "head": json.loads((directory / "head.json").read_text())}
(directory / "results.json").write_text(json.dumps(result, indent=2) + "\n")
PY
