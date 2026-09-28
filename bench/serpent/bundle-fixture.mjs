// Bundle Serpent's own large-library fixture generator (TypeScript, extensionless
// imports) into a single ESM file that a plain Node/Electron process can import.
// Serpent's generator code is not modified — this only makes it loadable outside
// vitest, because sharp segfaults under ELECTRON_RUN_AS_NODE on Linux.
//
//   node bench/serpent/bundle-fixture.mjs
import { rolldown } from '../../reference/Serpent/node_modules/rolldown/dist/index.mjs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const serpentRoot = path.resolve(here, '../../reference/Serpent');

const build = await rolldown({
  cwd: serpentRoot,
  input: path.join(serpentRoot, 'tests/worker/large-library-fixture.ts'),
  platform: 'node',
  external: ['sharp', 'better-sqlite3', 'electron', '@napi-rs/canvas'],
  resolve: { extensions: ['.ts', '.tsx', '.js', '.mjs', '.json'] },
});

await build.write({
  file: path.join(here, 'fixture-bundle.mjs'),
  format: 'esm',
  sourcemap: false,
  codeSplitting: false,
});
console.log(`bundled -> ${path.join(here, 'fixture-bundle.mjs')}`);
