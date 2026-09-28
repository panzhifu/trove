// A second opinion on one row of the comparison: the search latency, measured
// on a query that matches a *small* slice of the library.
//
//   node bench/serpent/bundle-search-probe.mjs
//   SERPENT_PROBE_PATH=$PWD/bench/work/lib-20k \
//     reference/Serpent/node_modules/electron/dist/electron \
//     bench/serpent/search-probe.mjs
//
// Why this exists: Serpent's own bench asks for the token `asset`, which in the
// fixture describes nearly every row (all 20 000 descriptions contain it).
// Trove's full-text leg gathers candidates through `CANDIDATE_CAP` (2 000 in
// `crates/trove-core/src/search.rs`), so on a query that wide the two sides are
// ranking different amounts of work and the ratio means little. This probe asks
// the same `searchAssets` twice: once with that broad token, once with
// `serpent-large-library-needle`, which the fixture plants in 9 % of rows —
// inside the cap on the Trove side, so the second number is the one where both
// applications rank the same candidate set.
import { performance } from 'node:perf_hooks';

import { LibraryService } from '../../reference/Serpent/src/worker/library-service';

const fixturePath = process.env.SERPENT_PROBE_PATH;
if (!fixturePath) throw new Error('SERPENT_PROBE_PATH is required');

const service = new LibraryService({ observerFactory: () => ({ close() {} }) });
const opened = service.openLibrary(fixturePath);
const libId = { libraryId: opened.libraryId };

function bench(operation, samples = 5) {
  operation();
  const times = Array.from({ length: samples }, () => {
    const startedAt = performance.now();
    operation();
    return performance.now() - startedAt;
  });
  times.sort((a, b) => a - b);
  return Number(times[Math.floor(times.length / 2)].toFixed(2));
}

const results = {};
for (const [name, token] of [
  ['broadToken', 'asset'],
  ['needle', 'serpent-large-library-needle'],
  ['rareToken', 'serpent'],
]) {
  const page = service.searchAssets({
    ...libId,
    query: { clauses: [{ field: null, values: [token], exclude: false }] },
    limit: 50,
    offset: 0,
  });
  results[token === 'asset' ? 'searchBroadAsset' : name] = bench(() => service.searchAssets({
    ...libId,
    query: { clauses: [{ field: null, values: [token], exclude: false }] },
    limit: 50,
    offset: 0,
  }));
  results[`${name}Matches`] = page.total;
}
results.assets = service.searchAssets({ ...libId, limit: 1, offset: 0 }).total;

console.log(`SEARCH_PROBE_JSON ${JSON.stringify(results)}`);
service.closeAll();
