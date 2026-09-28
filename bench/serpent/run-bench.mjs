// Run Serpent's own worker-layer performance suites and collect the JSON lines
// they print. Nothing here reimplements a measurement: each suite is Serpent's
// test file, run by Serpent's own Electron-as-Node vitest runner.
//
//   node bench/serpent/run-bench.mjs <fixture-path> [--repeats N] [--out FILE]
//
// Emits one JSON object per metric per repeat into --out (default
// bench/results/serpent-<basename>.jsonl) so the caller can take medians.
import { spawnSync } from 'node:child_process';
import { appendFileSync, mkdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, '../..');
const serpentRoot = path.join(repoRoot, 'reference/Serpent');

const argv = process.argv.slice(2);
const fixture = argv[0];
if (!fixture) {
  console.error('usage: node bench/serpent/run-bench.mjs <fixture-path> [--repeats N] [--out FILE]');
  process.exit(2);
}
const flag = (name, fallback) => {
  const i = argv.indexOf(name);
  return i === -1 ? fallback : argv[i + 1];
};
const repeats = Number(flag('--repeats', '3'));
const fixturePath = path.resolve(fixture);
const out = path.resolve(
  flag('--out', path.join(repoRoot, `bench/results/serpent-${path.basename(fixturePath)}.jsonl`)),
);
mkdirSync(path.dirname(out), { recursive: true });

// Serpent's suites print their report on stdout: `comprehensive-perf-bench`
// prefixes one line with PERF_BENCH_JSON, `large-library-performance` prints
// bare JSON objects that carry a `suite` field.
const suites = [
  { test: 'tests/worker/comprehensive-perf-bench.test.ts', env: 'SERPENT_PERF_BENCH_PATH', marker: 'PERF_BENCH_JSON' },
  { test: 'tests/worker/large-library-performance.test.ts', env: 'SERPENT_LARGE_LIBRARY_PERF_PATH', marker: null },
];

for (const suite of suites) {
  for (let i = 0; i < repeats; i += 1) {
    const run = spawnSync(
      process.execPath,
      ['scripts/run-vitest-with-electron.mjs', 'run', '--config', 'vitest.config.ts', suite.test, '--disableConsoleIntercept'],
      {
        cwd: serpentRoot,
        encoding: 'utf8',
        timeout: 900_000,
        env: { ...process.env, [suite.env]: fixturePath },
      },
    );
    const payload = pickReport(run.stdout ?? '', suite.marker);
    if (!payload) {
      console.error(`${suite.test} repeat ${i + 1}: no report line in output`);
      console.error((run.stdout ?? '') + (run.stderr ?? '').slice(-1500));
      process.exitCode = 1;
      continue;
    }
    payload._suite = path.basename(suite.test, '.test.ts');
    payload._repeat = i + 1;
    appendFileSync(out, `${JSON.stringify(payload)}\n`);
    console.log(`${payload._suite}/${payload.suite ?? '-'} repeat ${i + 1} -> ${out}`);
  }
}

function pickReport(stdout, marker) {
  for (const line of stdout.split('\n')) {
    if (marker) {
      if (line.startsWith(marker)) return JSON.parse(line.slice(marker.length + 1));
      continue;
    }
    const trimmed = line.trim();
    if (!trimmed.startsWith('{') || !trimmed.endsWith('}')) continue;
    try {
      const parsed = JSON.parse(trimmed);
      if (parsed && typeof parsed === 'object' && 'suite' in parsed) return parsed;
    } catch {
      // Vitest writes unrelated braces to stdout; only the report parses whole.
    }
  }
  return null;
}
