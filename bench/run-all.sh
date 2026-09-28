#!/usr/bin/env bash
# One command to reproduce the whole Trove ↔ Serpent comparison.
#
#   bash bench/run-all.sh            # everything
#   STAGES=aggregate bash bench/run-all.sh   # re-render the tables only
#
# Stages: fixture · serpent · trove · proc · aggregate
# Each writes into bench/results/ and bench/work/ (both disposable); nothing
# here touches a real library or the user's configuration.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
WORK=$ROOT/bench/work
RESULTS=$ROOT/bench/results
ELECTRON=$ROOT/reference/Serpent/node_modules/electron/dist/electron
STAGES=${STAGES:-fixture serpent trove proc aggregate}
mkdir -p "$WORK" "$RESULTS"

has() { [[ " $STAGES " == *" $1 "* ]]; }

# ---------------------------------------------------------------- fixture ---
if has fixture; then
  # The bundle is generated from Serpent's own TypeScript so the fixture stays
  # theirs; see bench/serpent/README.md for why it cannot run under vitest here.
  node bench/serpent/bundle-fixture.mjs
  for spec in "lib-20k 20000" "lib-100k 100000"; do
    set -- $spec
    name=$1; assets=$2
    if [[ -f "$WORK/$name/.serpent/large-library-fixture.json" ]]; then
      echo "fixture $name already present"
      continue
    fi
    echo "generating $name ($assets assets, ~$((assets * 145 / 100000)) GiB of files)"
    FIX_OUT=$WORK/$name FIX_ASSETS=$assets FIX_RESET=1 \
      "$ELECTRON" bench/serpent/gen-fixture.cjs --no-sandbox | tail -1
  done
fi

# ----------------------------------------------------------------- serpent ---
if has serpent; then
  for name in lib-20k lib-100k; do
    [[ -d $WORK/$name ]] || continue
    node bench/serpent/run-bench.mjs "$WORK/$name" --repeats "${REPEATS:-3}" \
      --out "$RESULTS/serpent-$name.jsonl"
  done
fi

# ------------------------------------------------------------------- trove ---
if has trove; then
  cargo build --release -p trove-core --example serpent_parity_bench
  for name in lib-20k lib-100k; do
    [[ -d $WORK/$name ]] || continue
    out=$("$ROOT/target/release/examples/serpent_parity_bench" "$WORK/$name" "${ROUNDS:-5}")
    printf '%s\n' "$out" | sed -n 's/^PARITY_JSON //p' >> "$RESULTS/trove-$name.jsonl"
    printf '%s\n' "$out" | sed -n '1,/^PARITY_JSON/p' | grep -v '^PARITY_JSON' > "$RESULTS/trove-$name.txt"
  done
fi

# -------------------------------------------------------------------- proc ---
if has proc; then
  # Both applications are measured by the same external detector: niri reports
  # the moment a window is mapped, /proc reports what the tree costs. Serpent
  # needs --ozone-platform=x11 here because its Wayland path never maps a
  # window under niri on this machine.
  seed_trove_profile() {
    local dir=$1 link=$2
    rm -rf "$dir"
    mkdir -p "$dir/config" "$dir/data/libraries" "$dir/cache/libraries"
    printf '{"libraries":[{"slug":"bench","name":"bench"}],"active_library":"bench","language":"zh-CN","collect_enabled":false,"keybindings":{}}\n' \
      > "$dir/config/config.json"
    [[ $link == none ]] || { ln -sfn "$link" "$dir/data/libraries/bench"; ln -sfn "$link/cache" "$dir/cache/libraries/bench"; }
  }
  seed_serpent_profile() {
    local dir=$1 link=$2
    rm -rf "$dir"; mkdir -p "$dir/user-data"
    [[ $link == none ]] || python3 - "$dir/user-data/recent-library.json" "$link" <<'PY'
import json, sys, time
open(sys.argv[1], 'w').write(json.dumps({
    "version": 2, "activePath": sys.argv[2],
    "libraries": [{"path": sys.argv[2], "name": "bench", "lastOpenedAt": time.strftime('%Y-%m-%dT%H:%M:%S.000Z')}]}) + "\n")
PY
  }

  : > "$RESULTS/proc.jsonl"
  for spec in "Trove lib-20k" "TroveEmpty none" "Serpent lib-20k" "SerpentEmpty none"; do
    set -- $spec
    label=$1; fixture=$2
    for i in 1 2 3; do
      if [[ $label == Trove* ]]; then
        seed_trove_profile "$WORK/gui-$label" "$([[ $fixture == none ]] && echo none || echo "$WORK/$fixture.trove-mirror")"
        node bench/process-bench.mjs --label "$label" --window 'trove|Trove' --hold 25000 --sample 50 \
          --env "TROVE_CONFIG_DIR=$WORK/gui-$label/config" \
          --env "TROVE_DATA_DIR=$WORK/gui-$label/data" \
          --env "TROVE_CACHE_DIR=$WORK/gui-$label/cache" \
          -- ./target/release/trove-app
      else
        serpent_fixture=$([[ $fixture == none ]] && echo none || echo "$WORK/$fixture")
        seed_serpent_profile "$WORK/gui-$label" "$serpent_fixture"
        node bench/process-bench.mjs --label "$label" --window 'serpent|Serpent' --hold 40000 --sample 100 \
          --cwd "$ROOT/reference/Serpent" \
          --env SERPENT_E2E=1 --env SERPENT_E2E_RESTORE_RECENT=1 \
          --env "SERPENT_E2E_OPEN_LIBRARY_PATH=$serpent_fixture" \
          --env "SERPENT_E2E_USER_DATA_PATH=$WORK/gui-$label/user-data" \
          --env SERPENT_PREVIEW_CACHE_FORCE=1 \
          -- ./node_modules/electron/dist/electron . --no-sandbox --ozone-platform=x11
      fi
    done >> "$RESULTS/proc.jsonl" 2>&1
  done
  grep -o '{"label".*}' "$RESULTS/proc.jsonl" > "$RESULTS/proc.tmp" && mv "$RESULTS/proc.tmp" "$RESULTS/proc.jsonl"
fi

# --------------------------------------------------------------- aggregate ---
if has aggregate; then
  node bench/aggregate.mjs --dir "$RESULTS" --out "$RESULTS/summary.md"
fi
