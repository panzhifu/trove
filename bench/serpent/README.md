# bench/serpent — 在 Linux 上驱动 Serpent 自带的基准

`reference/Serpent` 是上游 [dolag233/Serpent](https://github.com/dolag233/Serpent) 的本地克隆（v0.2.9）。
它自带一整套 worker 层基准，本来就能跑；只有**生成 fixture** 这一步在本机跑不通，所以这里的
脚本只做一件事：把 Serpent 自己的生成代码换到一个能跑起来它自己依赖的运行时里。**没有改动
`reference/Serpent` 的任何一行源码**，改动只发生在"从哪个进程加载它"。

## 为什么要绕

`npm run large-library:generate` 走 `scripts/run-vitest-with-electron.mjs`，它用
`ELECTRON_RUN_AS_NODE=1` 把 Electron 当 Node 跑 vitest。Serpent 的 fixture 图片由 **sharp**
编码（约 2 MB 的噪声图，一个 20k 库共 29 GB），而 sharp 在这种模式下会直接段错误：

```console
$ ELECTRON_RUN_AS_NODE=1 node_modules/electron/dist/electron probe.cjs
c: sharp require ok          ← require 本身不崩
d: sharp sync-buffer ok      ← 一旦真的编码就崩
Segmentation fault (core dumped)      exit 139
```

崩在 sharp 自己警告过的那条路径上（`[SharpElectronLinux] Warning: Binaries provided by Electron
for use on Linux may be incompatible with sharp`）。同一个 sharp 在**真正的 Electron 主进程**里
是好的：

```console
$ node_modules/electron/dist/electron probe-main.cjs      # app.on('ready') 里编码
SHARP-IN-MAIN-OK bytes=364
SQLITE-IN-MAIN-OK                                          # better-sqlite3 + FTS5 也正常
```

所以分工是：

| 步骤 | 需要的原生模块 | 跑在哪 |
|---|---|---|
| 生成 fixture（写 2 万个真实文件 + 建库） | sharp **和** better-sqlite3 | 真 Electron 主进程（本目录的 `gen-fixture.cjs`） |
| 跑基准（只读库、只查 SQLite） | better-sqlite3 | Serpent 原样命令，`ELECTRON_RUN_AS_NODE` 下没问题 |

基准阶段完全不碰 sharp，因此 Serpent 的 `comprehensive-perf-bench` 与
`large-library-performance` 都用**它自己的** `npm`/vitest 入口跑，一条命令没改。

## 三个文件

- `bundle-fixture.mjs` — 用仓库里现成的 rolldown 把 `tests/worker/large-library-fixture.ts`
  打成单文件 ESM（那份 TS 用无扩展名的相对 import，Node 自带的类型剥离加载不了）。
  sharp / better-sqlite3 / electron 留作 external，运行时仍从 `reference/Serpent/node_modules` 解析。
- `gen-fixture.cjs` — 真 Electron 主进程驱动，`app.on('ready')` 里 import 上面那个 bundle，
  调 **未经修改的** `ensureLargeLibraryFixture()`。环境变量：`FIX_OUT` `FIX_ASSETS` `FIX_SEED`
  `FIX_PROFILE` `FIX_RESET` `FIX_WRITE_FILES`。
- `run-bench.mjs` — 跑 Serpent 自己的两个 worker 基准，把它们打印的 JSON 收进
  `bench/results/serpent-<fixture>.jsonl`。

```console
node bench/serpent/bundle-fixture.mjs
FIX_OUT=$PWD/bench/work/lib-20k FIX_ASSETS=20000 FIX_RESET=1 \
  reference/Serpent/node_modules/electron/dist/electron bench/serpent/gen-fixture.cjs --no-sandbox
node bench/serpent/run-bench.mjs bench/work/lib-20k --repeats 3
```

## `FIX_WRITE_FILES=0` 的坑

`ensureLargeLibraryFixture` 的 `writeFiles: false` 看起来是"只要库不要文件"的省钱开关，实际不止：
那份代码里 `byte_size` 取的是 `byteSizes.get(rel) ?? (1024 + index % 97)`，
而 `revision_artifacts`（宽高、时长）也只在 `writeFiles` 为真时写入。也就是说关掉写文件会同时把
每个资产的体积退化成 ~1 KB、把宽高清零——查询层里的瀑布流几何那一档就变成在量空数据。
所以 100k 那轮仍然带文件生成（143 GB，约 2 分钟），生成后库本身只有 400 MB。

## 另一处环境事实

Serpent 的 GUI 在本机（Arch + niri，Wayland）用默认 ozone 后端**永远不会 map 窗口**：它建了
surface、日志里只有几条 `wayland_surface.cc` 的能力警告，然后停在 `show: false` 等
`ready-to-show`。加 `--ozone-platform=x11` 走 XWayland 就正常出窗。进程层测量因此是在 X11
后端下做的，这一点在报告里和数字一起写了。
