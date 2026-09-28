// Run Serpent's own large-library fixture generator inside a real Electron main
// process.
//
// Why not `npm run large-library:generate`: that path goes through
// scripts/run-vitest-with-electron.mjs, which runs Electron with
// ELECTRON_RUN_AS_NODE=1. sharp segfaults the worker in that mode on Linux
// (the SharpElectronLinux warning sharp prints is exactly this case), and the
// generator needs sharp to encode the ~2 MB noise images the fixture is made
// of. In a normal Electron main process the same sharp build works, so this
// driver only changes *where* the generator runs — the generator itself is
// Serpent's tests/worker/large-library-fixture.ts, unmodified, loaded through
// the rolldown bundle that bundle-fixture.mjs writes next to this file.
//
//   FIX_OUT=bench/work/lib-20k FIX_ASSETS=20000 \
//     reference/Serpent/node_modules/electron/dist/electron bench/serpent/gen-fixture.cjs --no-sandbox
const path = require('node:path');
const { app } = require('electron');

app.commandLine.appendSwitch('no-sandbox');

const OUT = process.env.FIX_OUT;
const ASSETS = Number(process.env.FIX_ASSETS ?? 20000);
const SEED = Number(process.env.FIX_SEED ?? 20260816);
const PROFILE = process.env.FIX_PROFILE ?? 'mixed';
const RESET = process.env.FIX_RESET === '1';
// Off for the 100k round: the query layer reads the library database only, and
// 100k real files would cost ~190 GB of disk that changes no measurement.
const WRITE_FILES = process.env.FIX_WRITE_FILES !== '0';

if (!OUT) {
  console.log('FIXTURE_FAIL FIX_OUT is required');
  app.exit(2);
}

app.on('ready', async () => {
  const t0 = Date.now();
  try {
    const mod = await import(
      path.join(__dirname, 'fixture-bundle.mjs').replace(/^/, 'file://')
    );
    const manifest = await mod.ensureLargeLibraryFixture({
      outputPath: path.resolve(OUT),
      assetCount: ASSETS,
      seed: SEED,
      reset: RESET,
      assetProfile: PROFILE,
      writeFiles: WRITE_FILES,
    });
    console.log(
      'FIXTURE_OK ' +
        JSON.stringify({ ...manifest, seconds: Number(((Date.now() - t0) / 1000).toFixed(1)) }),
    );
    app.exit(0);
  } catch (e) {
    console.log('FIXTURE_FAIL ' + (e && e.stack ? e.stack : String(e)));
    app.exit(1);
  }
});
