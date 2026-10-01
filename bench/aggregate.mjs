// Merge the two applications' benchmark output into one paired table.
//
//   node bench/aggregate.mjs [--dir bench/results] [--out bench/results/summary.md]
//
// Reads what the runners wrote next to it:
//   serpent-<fixture>.jsonl   one line per repeat from bench/serpent/run-bench.mjs
//   trove-<fixture>.jsonl     PARITY_JSON lines from crates/trove-core/examples
//   proc.jsonl                PROC_JSON lines from bench/process-bench.mjs
//
// Metric names are paired only where the two sides genuinely ask the same
// question. Everything either side measures alone is dumped under its own
// heading rather than given a ratio, because a number divided by a different
// number is worse than no number.
import { readdirSync, readFileSync, existsSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const arg = (name, fallback) => {
  const i = process.argv.indexOf(name);
  return i === -1 ? fallback : process.argv[i + 1];
};
const dir = path.resolve(arg('--dir', path.join(here, 'results')));
const out = arg('--out', null);

// Serpent's key → Trove's key, with the question both sides are asking.
// `note` records the differences a reader would otherwise smooth over.
const PAIRS = [
  ['openLibraryMs', 'openLibraryMs', '打开库（冷）', '两边都是冷打开同一个库数据库'],
  ['allBrowseFirstPageMs', 'allBrowseFirstPageMs', '默认浏览首页 50 行', '两边都含精确总数'],
  ['deepOffsetPageMs', 'deepOffsetPageMs', '深翻页 offset 10000', ''],
  ['collectionSwitchMs', 'collectionSwitchMs', '切进合集', '成员数实读自 Trove 镜像（collections::list 取第一个合集）'],
  ['folderSwitchMs', 'folderSwitchMs', '切进文件夹（一层）', 'Serpent 走 managed_folder_id 外键；Trove 走 source_path 生成列上的前缀索引'],
  ['collectionRecursiveSwitchMs', 'folderSwitchRecursiveMs', '递归一层子树', '松配对：Serpent 是递归合集，Trove 侧栏没有递归合集，取子树文件夹'],
  ['searchFixedTokenMs', 'searchFixedTokenMs', '全文检索高频词 asset', '同一词、同一批行'],
  ['layoutOnlyMs', 'layoutOnlyMs', '整表瀑布流几何', '两边各一次给出整表几何；Trove 走 store 窄投影（只读 id/width/height）+ 同一 justify'],
  ['sortNameAscMs', 'sortNameAscMs', '按名称排序', ''],
  ['sortCreatedAtDescMs', 'sortCreatedAtDescMs', '按创建时间倒序', ''],
  ['sortModifiedDescMs', 'sortUpdatedAtDescMs', '按修改时间倒序', ''],
  ['sortByteSizeDescMs', 'sortByteSizeDescMs', '按体积倒序', ''],
  ['sortRatingDescMs', 'sortRatingDescMs', '按评分倒序', ''],
  ['filterRatingMs', 'filterRatingMs', '筛选评分 ≥3', ''],
  ['inspectorMetadataMs', 'inspectorMetadataMs', '检查器一次读', ''],
  ['needle', 'searchNeedleMs', '选择性检索 needle（1826 命中）', '两边都远低于各自的候选上限，是真正同题的那一行；来自 bench/serpent/search-probe.ts'],
  ['sidebarListFoldersMs', 'sidebarListFoldersMs', '侧栏文件夹一次读', '口径不同：Serpent 列出 160 行文件夹记录，Trove 扫一遍 source_path 索引再现场归并'],
  ['sidebarListCollectionsMs', 'sidebarListCollectionsMs', '侧栏合集一次读', ''],
  ['browseSessionOpenMs', 'browseSessionOpenMs', '冻结分页会话', '来自 Serpent 的 large-library-performance', 'large-library-performance'],
  ['browseSessionPageMs', 'browseSessionPageMs', '会话内取一窗', '来自 Serpent 的 large-library-performance', 'large-library-performance'],
];

const PROC_ROWS = [
  ['mapMs', '出窗（spawn → 合成器报告窗口）', 'ms'],
  ['settleMs', '静止（CPU 安静 1 s）', 'ms'],
  ['cpuMs', '启动期 CPU 时间', 'ms'],
  ['processes', '进程数', ''],
  ['threads', '线程数', ''],
  ['rssMb', '常驻集 RSS', 'MB'],
  ['pssMb', '比例常驻 PSS', 'MB'],
  ['peakPssMb', '峰值 PSS', 'MB'],
];

function readJsonl(file) {
  if (!existsSync(file)) return [];
  return readFileSync(file, 'utf8')
    .split('\n')
    .filter((line) => line.trim().startsWith('{') || line.includes('_JSON '))
    .map((line) => {
      try {
        return JSON.parse(line.slice(line.indexOf('{')));
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

function median(values) {
  const nums = values.filter((v) => typeof v === 'number' && Number.isFinite(v)).sort((a, b) => a - b);
  return nums.length ? nums[Math.floor(nums.length / 2)] : null;
}

function merge(files, suite) {
  let rows = files.flatMap(readJsonl);
  if (suite) rows = rows.filter((r) => r._suite === suite);
  const buckets = new Map();
  for (const row of rows) {
    for (const [key, value] of Object.entries(row)) {
      if (typeof value !== 'number') continue;
      if (!buckets.has(key)) buckets.set(key, []);
      buckets.get(key).push(value);
    }
  }
  const merged = new Map();
  for (const [key, values] of buckets) merged.set(key, median(values));
  return merged;
}

function mergeByLabel(files) {
  const rows = files.flatMap(readJsonl);
  const labels = new Map();
  for (const row of rows) {
    const label = row.label ?? 'default';
    if (!labels.has(label)) labels.set(label, []);
    labels.get(label).push(row);
  }
  const merged = new Map();
  for (const [label, rowsForLabel] of labels) {
    const fields = new Map();
    for (const [key, value] of Object.entries(rowsForLabel[0])) {
      if (typeof value === 'number') fields.set(key, median(rowsForLabel.map((r) => r[key])));
    }
    merged.set(label, fields);
  }
  return merged;
}

const present = readdirSync(dir).filter((f) => f.endsWith('.jsonl'));
// One table per fixture: a 20k median and a 100k median are different answers
// and must never be averaged together.
const fixtures = [...new Set(present.map((f) => f.replace(/^(serpent|trove|proc)-?/, '').replace(/\.jsonl$/, '')).filter((f) => f && f !== 'proc'))];
if (!fixtures.length) fixtures.push('');

const proc = mergeByLabel(present.filter((f) => f.startsWith('proc')).map((f) => path.join(dir, f)));

const f = (v) => (v === null || v === undefined ? '—' : Number.isInteger(v) ? String(v) : v.toFixed(2));
const lines = [];

for (const fixture of fixtures) {
  const serpentFiles = present.filter((x) => x.startsWith(`serpent-${fixture}`) && x.endsWith('.jsonl')).map((x) => path.join(dir, x));
  const serpent = merge(serpentFiles);
  const serpentBySuite = new Map(
    ['comprehensive-perf-bench', 'large-library-performance'].map((name) => [name, merge(serpentFiles, name)]),
  );
  const trove = merge(present.filter((x) => x.startsWith(`trove-${fixture}`) && x.endsWith('.jsonl')).map((x) => path.join(dir, x)));
  if (!serpent.size && !trove.size) continue;
  lines.push(
    `## ${fixture || '结果'} · 查询与浏览层（ms，越小越好；倍数 = Serpent ÷ Trove）`,
    '',
    '| 指标 | Serpent | Trove | 倍数 | 口径 |',
    '|---|---:|---:|---:|---|',
  );
  for (const [sKey, tKey, label, note, suite] of PAIRS) {
    const src = suite ? serpentBySuite.get(suite) ?? new Map() : serpent;
    const s = src.has(sKey) ? src.get(sKey) : serpent.get(sKey);
    const t = trove.has(tKey) ? trove.get(tKey) : null;
    const ratio = s !== null && t !== null && t > 0 ? (s / t).toFixed(2) + '×' : '—';
    // The one label that is data rather than wording: which collection the
    // switch actually measured, reported by the bench that measured it.
    const rowLabel =
      tKey === 'collectionSwitchMs' && trove.get('collectionMembers') != null
        ? `${label}（${trove.get('collectionMembers')} 成员）`
        : label;
    lines.push(`| ${rowLabel} | ${f(s)} | ${f(t)} | ${ratio} | ${note} |`);
  }
  const paired = new Set(PAIRS.flatMap(([a, b]) => [a, b]));
  const extra = (map, name) => {
    const keys = [...map.keys()]
      .filter((k) => !paired.has(k) && !k.endsWith('Min') && !k.endsWith('Max') && !['_repeat', 'assets', 'targetAssets', 'liveAssets', 'mirrorDbBytes', 'collectionMembers'].includes(k))
      .sort();
    if (keys.length) lines.push('', `${name}（对方无对应项）: ` + keys.map((k) => `${k}=${f(map.get(k))}`).join(' · '));
  };
  extra(serpent, 'Serpent 独有');
  extra(trove, 'Trove 独有');
  lines.push('');
}

if (proc.size) {
  const order = [...proc.keys()].sort();
  lines.push('## 进程层', '', '| 指标 | ' + order.join(' | ') + ' |', '|---|' + order.map(() => '---:|').join(''));
  for (const [key, label, unit] of PROC_ROWS) {
    const cells = order.map((who) => f(proc.get(who).get(key)));
    if (cells.every((c) => c === '—')) continue;
    lines.push(`| ${label}${unit ? ` (${unit})` : ''} | ${cells.join(' | ')} |`);
  }
}

const report = lines.join('\n');
console.log(report);
if (out) writeFileSync(out, report + '\n');
