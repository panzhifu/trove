// Process-level measurement for a desktop application: how long it takes to
// put a window on screen, and what it costs to sit there.
//
//   node bench/process-bench.mjs --label Trove --window 'Trove|trove' \
//     --env TROVE_DATA_DIR=... --hold 20000 -- target/release/trove-app
//
// The same detector runs against both applications on purpose. Neither is
// measured from inside: the clock starts at `spawn`, the first-frame moment is
// the moment the compositor (`niri msg --json windows`) reports a window whose
// app_id or title matches --window, and memory is read from /proc. That makes
// the two sides comparable without either application having to expose a
// hook — and it means the number includes everything an application does before
// it shows anything, which for an Electron shell is the whole point.
//
// What is recorded:
//   mapMs      spawn → the window exists in the compositor
//   settleMs   spawn → the process tree stopped burning CPU (CPU-quiet for
//              1 s, at least 250 ms after the window mapped)
//   cpuMs      tree CPU seconds consumed over the sampling window
//   rssMb      sum of resident set size across the tree (shared libraries
//              counted once per process)
//   pssMb      sum of proportional set size: shared pages divided between the
//              processes mapping them. This is the number to compare across a
//              one-process and a multi-process application.
//   peakPssMb  the same, at its worst moment before settling
//
// Output is one `PROC_JSON` line, so bench/aggregate.mjs can merge it.
import { spawn } from 'node:child_process';
import { readdirSync, readFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';

const argv = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = argv.indexOf(name);
  return i === -1 ? fallback : argv[i + 1];
};
const flagAll = (name) => {
  const out = [];
  argv.forEach((value, i) => {
    if (value === name && argv[i + 1]) out.push(argv[i + 1]);
  });
  return out;
};

const label = flag('--label', 'app');
// Whole-string match, on either identity the compositor reports. A substring
// test is not safe here: a browser tab titled "panzhifu/trove: trove" would
// otherwise be read as the Trove window having mapped instantly.
const windowPattern = new RegExp(`^(?:${flag('--window', '.')})$`, 'i');
const holdMs = Number(flag('--hold', '20000'));
const sampleMs = Number(flag('--sample', '50'));
const dwellMs = Number(flag('--dwell', '5000'));
const dashDash = argv.indexOf('--');
if (dashDash === -1 || dashDash === argv.length - 1) {
  console.error('usage: process-bench.mjs --label NAME --window REGEX -- <command...>');
  process.exit(2);
}
const command = argv.slice(dashDash + 1);
const env = { ...process.env, ...(process.env.BENCH_EXTRA_ENV ? JSON.parse(process.env.BENCH_EXTRA_ENV) : {}) };
for (const pair of flagAll('--env')) {
  const at = pair.indexOf('=');
  env[pair.slice(0, at)] = pair.slice(at + 1);
}

const PAGE = 4096;

function tree(rootPid) {
  const found = new Set();
  const stack = [rootPid];
  while (stack.length) {
    const pid = stack.pop();
    if (found.has(pid)) continue;
    found.add(pid);
    try {
      const stat = readFileSync(`/proc/${pid}/stat`, 'utf8');
      // comm is parenthesised and may contain spaces: parse after the last ')'.
      const rest = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
      stack.push(...childrenOf(pid));
      void rest;
    } catch {
      // already gone
    }
  }
  return [...found].filter((pid) => {
    try {
      readFileSync(`/proc/${pid}/stat`, 'utf8');
      return true;
    } catch {
      return false;
    }
  });
}

function childrenOf(parentPid) {
  const out = [];
  for (const entry of readdirSync('/proc')) {
    if (!/^\d+$/.test(entry)) continue;
    try {
      const stat = readFileSync(`/proc/${entry}/stat`, 'utf8');
      const ppid = Number(stat.slice(stat.lastIndexOf(')') + 2).split(' ')[1]);
      if (ppid === parentPid) out.push(Number(entry));
    } catch {
      // raced with exit
    }
  }
  return out;
}

function sample(pids) {
  let rss = 0;
  let pss = 0;
  let cpuTicks = 0;
  let threads = 0;
  for (const pid of pids) {
    try {
      const statm = readFileSync(`/proc/${pid}/statm`, 'utf8').split(' ');
      rss += Number(statm[1]) * PAGE;
      const rollup = readFileSync(`/proc/${pid}/smaps_rollup`, 'utf8');
      const match = rollup.match(/^Pss:\s+(\d+) kB$/m);
      if (match) pss += Number(match[1]) * 1024;
      const stat = readFileSync(`/proc/${pid}/stat`, 'utf8');
      const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
      cpuTicks += Number(fields[11]) + Number(fields[12]);
      threads += Number(fields[17]);
    } catch {
      // process gone between listing and reading
    }
  }
  return { rss, pss, cpuTicks, threads };
}

function mappedWindows() {
  try {
    const raw = execFileSync('niri', ['msg', '--json', 'windows'], { encoding: 'utf8', timeout: 2000 });
    return JSON.parse(raw);
  } catch {
    return [];
  }
}

// A window that already exists makes every number below wrong in the same
// direction — the map moment reads as ~0 ms. Fail loudly instead.
const strays = mappedWindows().filter(
  (w) => windowPattern.test(w.app_id ?? '') || windowPattern.test(w.title ?? ''),
);
if (strays.length) {
  console.error(
    `refusing to measure: ${strays.length} window(s) matching --window '${windowPattern.source}' are already mapped ` +
      `(pids ${strays.map((w) => w.pid).join(', ')}). Close them first.`,
  );
  process.exit(3);
}

const startedAt = Date.now();
const child = spawn(command[0], command.slice(1), {
  env,
  cwd: flag('--cwd', process.cwd()),
  detached: true,
  stdio: 'ignore',
});
const clock = () => Date.now() - startedAt;

let mapMs = null;
let settleAt = null;
let lastCpuTicks = 0;
let cpuQuietSince = null;
let peakPss = 0;
let last = { rss: 0, pss: 0, cpuTicks: 0, threads: 0 };
let samples = 0;
let processesSeen = 0;

await new Promise((resolve) => {
  const timer = setInterval(() => {
    const pids = tree(child.pid);
    processesSeen = Math.max(processesSeen, pids.length);
    last = sample(pids);
    samples += 1;
    peakPss = Math.max(peakPss, last.pss);
    const now = clock();
    if (mapMs === null) {
      const hit = mappedWindows().some(
        (w) => windowPattern.test(w.app_id ?? '') || windowPattern.test(w.title ?? ''),
      );
      if (hit) mapMs = now;
    }
    if (last.cpuTicks > lastCpuTicks + 2) cpuQuietSince = null;
    else if (cpuQuietSince === null && last.cpuTicks === lastCpuTicks) cpuQuietSince = now;
    lastCpuTicks = last.cpuTicks;
    if (settleAt === null && mapMs !== null && cpuQuietSince !== null
        && now - cpuQuietSince >= 1000 && now - mapMs >= 250) {
      settleAt = cpuQuietSince;
    }
    // Report memory after a fixed dwell past the settle point. Reading it at the
    // moment CPU goes quiet instead catches the tree mid-load, and then "idle
    // with a 20k library open" and "idle at an empty profile" are sampled at
    // different points of their own start-up — which is how a 20k library
    // briefly looked lighter than no library at all.
    const stopAt = settleAt !== null ? settleAt + dwellMs : holdMs;
    if (now >= stopAt) {
      clearInterval(timer);
      resolve();
    }
  }, sampleMs);
});

const seconds = samples * sampleMs / 1000;
const result = {
  label,
  mapMs,
  settleMs: settleAt ?? (settleAt === 0 ? 0 : null),
  settled: settleAt !== null,
  cpuMs: Math.round((last.cpuTicks / 100) * 1000),
  cpuPerCorePct: Number(((last.cpuTicks / 100 / seconds) * 100).toFixed(1)),
  processes: processesSeen,
  threads: last.threads,
  rssMb: Number((last.rss / (1 << 20)).toFixed(1)),
  pssMb: Number((last.pss / (1 << 20)).toFixed(1)),
  peakPssMb: Number((peakPss / (1 << 20)).toFixed(1)),
  sampleWindowS: Number(seconds.toFixed(2)),
  dwellMs,
};

try {
  process.kill(-child.pid, 'SIGTERM');
} catch {
  /* already gone */
}
await new Promise((r) => setTimeout(r, 1200));
try {
  process.kill(-child.pid, 'SIGKILL');
} catch {
  /* already gone */
}

console.log(`PROC_JSON ${JSON.stringify(result)}`);
