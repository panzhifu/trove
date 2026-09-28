// Bundle bench/serpent/search-probe.ts (which imports Serpent's own
// LibraryService straight from source, extensionless relative imports and all)
// into one ESM file the Electron-as-Node runtime can run. Same trick as
// bundle-fixture.mjs, same reason: rolldown resolves what Node cannot.
//
//   node bench/serpent/bundle-fixture.mjs
import { rolldown } from '../../reference/Serpent/node_modules/rolldown/dist/index.mjs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const serpentRoot = path.resolve(here, '../../reference/Serpent');

const build = await rolldown({
  cwd: serpentRoot,
  input: path.join(here, 'search-probe.ts'),
  platform: 'node',
  external: ['sharp', 'better-sqlite3', 'electron', '@napi-rs/canvas'],
  resolve: { extensions: ['.ts', '.tsx', '.js', '.mjs', '.json'] },
});

await build.write({
  file: path.join(here, 'search-probe-bundle.mjs'),
  format: 'esm',
  sourcemap: false,
  codeSplitting: false,
});
console.log(`bundled -> ${path.join(here, 'search-probe-bundle.mjs')}`);
