## lib-100k · 查询与浏览层（ms，越小越好；倍数 = Serpent ÷ Trove）

| 指标 | Serpent | Trove | 倍数 | 口径 |
|---|---:|---:|---:|---|
| 打开库（冷） | 8.20 | 2.25 | 3.64× | 两边都是冷打开同一个库数据库 |
| 默认浏览首页 50 行 | 113.01 | 26.93 | 4.20× | 两边都含精确总数 |
| 深翻页 offset 10000 | 137.93 | 28.28 | 4.88× |  |
| 切进合集（1184 成员） | 35.25 | 172.73 | 0.20× |  |
| 切进文件夹（一层） | 2.09 | 66.45 | 0.03× | Serpent 走 folder 外键；Trove 走 json_extract(source_path) LIKE 前缀 |
| 递归一层子树 | 420.10 | 50.78 | 8.27× | 松配对：Serpent 是递归合集，Trove 侧栏没有递归合集，取子树文件夹 |
| 全文检索高频词 asset | 539.50 | 43.04 | 12.53× | 同一词、同一批行 |
| 整表瀑布流几何 | 625.95 | 399.36 | 1.57× |  |
| 按名称排序 | 112.35 | 27.20 | 4.13× |  |
| 按创建时间倒序 | 110.96 | 27.45 | 4.04× |  |
| 按修改时间倒序 | 405.24 | 58.34 | 6.95× |  |
| 按体积倒序 | 418.36 | 26.32 | 15.90× |  |
| 按评分倒序 | 430.98 | 26.44 | 16.30× |  |
| 筛选评分 ≥3 | 241.35 | 8.03 | 30.06× |  |
| 检查器一次读 | 0.17 | 0.04 | 4.25× |  |
| 选择性检索 needle（1826 命中） | — | 17.46 | NaN× | 两边都远低于各自的候选上限，是真正同题的那一行；来自 bench/serpent/search-probe.ts |
| 侧栏文件夹一次读 | 109.19 | 89.93 | 1.21× | 口径不同：Serpent 列出 160 行文件夹记录，Trove 从 2 万行 json_extract 现场归并 |
| 侧栏合集一次读 | 0.22 | 0.04 | 5.50× |  |
| 冻结分页会话 | 105.90 | 26.63 | 3.98× | 来自 Serpent 的 large-library-performance |
| 会话内取一窗 | 110.40 | 28.94 | 3.81× | 来自 Serpent 的 large-library-performance |

Serpent 独有（对方无对应项）: allBrowseMs=107.60 · browseSessionGeometryMs=109.30 · collectionRecursiveLayoutMs=823.10 · importPrepareFiveFilesMs=44.87 · inspectorMs=0.20 · layoutMs=623.40 · navigationSummaryInitialMs=489.50 · navigationSummaryWarmMs=0.10 · searchMs=149.10 · startupMs=9

Trove 独有（对方无对应项）: allBrowseFirstPageNoCountMs=0.16 · browseFirstPageAscMs=98.80 · collectionSwitchNoCountMs=0.56 · countFloorMs=1.99 · deepOffsetPageNoCountMs=1.55 · exactCountOnlyMs=26.35 · extraBytesPerRow=387.10 · filterKindImageMs=28.45 · filterKindImageNoCountMs=0.16 · floorWithStorePragmasMs=0.06 · folderSwitchNoCountMs=14.63 · folderSwitchRecursiveNoCountMs=0.51 · indexOnlySearchMs=38.48 · mirrorTotalRows=100000 · readFloorMs=0.16 · readFloorWithJsonMs=0.17 · sameSqlTroveConnMs=0.06 · scrollTwentyWindowsMs=32.05 · sqlFloorDeepPageMs=0.36 · sqlFloorFirstPageMs=0.06 · storeReadFirstPageMs=0.15 · typedFactsFloorMs=0.14

## lib-20k-search · 查询与浏览层（ms，越小越好；倍数 = Serpent ÷ Trove）

| 指标 | Serpent | Trove | 倍数 | 口径 |
|---|---:|---:|---:|---|
| 打开库（冷） | — | — | — | 两边都是冷打开同一个库数据库 |
| 默认浏览首页 50 行 | — | — | — | 两边都含精确总数 |
| 深翻页 offset 10000 | — | — | — |  |
| 切进合集（1184 成员） | — | — | — |  |
| 切进文件夹（一层） | — | — | — | Serpent 走 folder 外键；Trove 走 json_extract(source_path) LIKE 前缀 |
| 递归一层子树 | — | — | — | 松配对：Serpent 是递归合集，Trove 侧栏没有递归合集，取子树文件夹 |
| 全文检索高频词 asset | — | — | — | 同一词、同一批行 |
| 整表瀑布流几何 | — | — | — |  |
| 按名称排序 | — | — | — |  |
| 按创建时间倒序 | — | — | — |  |
| 按修改时间倒序 | — | — | — |  |
| 按体积倒序 | — | — | — |  |
| 按评分倒序 | — | — | — |  |
| 筛选评分 ≥3 | — | — | — |  |
| 检查器一次读 | — | — | — |  |
| 选择性检索 needle（1826 命中） | 23.82 | — | — | 两边都远低于各自的候选上限，是真正同题的那一行；来自 bench/serpent/search-probe.ts |
| 侧栏文件夹一次读 | — | — | — | 口径不同：Serpent 列出 160 行文件夹记录，Trove 从 2 万行 json_extract 现场归并 |
| 侧栏合集一次读 | — | — | — |  |
| 冻结分页会话 | — | — | — | 来自 Serpent 的 large-library-performance |
| 会话内取一窗 | — | — | — | 来自 Serpent 的 large-library-performance |

Serpent 独有（对方无对应项）: broadTokenMatches=20000 · needleMatches=1826 · rareToken=102.61 · rareTokenMatches=20000 · searchBroadAsset=88.14

## lib-20k · 查询与浏览层（ms，越小越好；倍数 = Serpent ÷ Trove）

| 指标 | Serpent | Trove | 倍数 | 口径 |
|---|---:|---:|---:|---|
| 打开库（冷） | 7.60 | 1.74 | 4.37× | 两边都是冷打开同一个库数据库 |
| 默认浏览首页 50 行 | 15.80 | 3.99 | 3.96× | 两边都含精确总数 |
| 深翻页 offset 10000 | 31.35 | 5.25 | 5.97× |  |
| 切进合集（1184 成员） | 7.20 | 15.76 | 0.46× |  |
| 切进文件夹（一层） | 1.41 | 15.12 | 0.09× | Serpent 走 folder 外键；Trove 走 json_extract(source_path) LIKE 前缀 |
| 递归一层子树 | 64.30 | 9.76 | 6.59× | 松配对：Serpent 是递归合集，Trove 侧栏没有递归合集，取子树文件夹 |
| 全文检索高频词 asset | 86.55 | 13.29 | 6.51× | 同一词、同一批行 |
| 整表瀑布流几何 | 152.07 | 55.18 | 2.76× |  |
| 按名称排序 | 15.85 | 3.88 | 4.09× |  |
| 按创建时间倒序 | 15.65 | 3.99 | 3.92× |  |
| 按修改时间倒序 | 57.82 | 9.36 | 6.18× |  |
| 按体积倒序 | 56.20 | 3.93 | 14.30× |  |
| 按评分倒序 | 59.53 | 3.84 | 15.50× |  |
| 筛选评分 ≥3 | 35.37 | 1.80 | 19.65× |  |
| 检查器一次读 | 0.15 | 0.05 | 3.00× |  |
| 选择性检索 needle（1826 命中） | 23.82 | 8.73 | 2.73× | 两边都远低于各自的候选上限，是真正同题的那一行；来自 bench/serpent/search-probe.ts |
| 侧栏文件夹一次读 | 18.84 | 17.52 | 1.08× | 口径不同：Serpent 列出 160 行文件夹记录，Trove 从 2 万行 json_extract 现场归并 |
| 侧栏合集一次读 | 0.24 | 0.04 | 6.00× |  |
| 冻结分页会话 | 16.20 | 3.57 | 4.54× | 来自 Serpent 的 large-library-performance |
| 会话内取一窗 | 16.40 | 5.51 | 2.98× | 来自 Serpent 的 large-library-performance |

Serpent 独有（对方无对应项）: allBrowseMs=16.70 · broadTokenMatches=20000 · browseSessionGeometryMs=18.10 · collectionRecursiveLayoutMs=109.20 · importPrepareFiveFilesMs=40.12 · inspectorMs=0.20 · layoutMs=153.90 · navigationSummaryInitialMs=84.90 · navigationSummaryWarmMs=0.10 · needleMatches=1826 · rareToken=102.61 · rareTokenMatches=20000 · searchBroadAsset=88.14 · searchMs=25.60 · startupMs=8.90

Trove 独有（对方无对应项）: allBrowseFirstPageNoCountMs=0.17 · browseFirstPageAscMs=17.22 · collectionSwitchNoCountMs=0.51 · countFloorMs=0.40 · deepOffsetPageNoCountMs=1.57 · exactCountOnlyMs=3.60 · extraBytesPerRow=76.40 · filterKindImageMs=4.43 · filterKindImageNoCountMs=0.17 · floorWithStorePragmasMs=0.06 · folderSwitchNoCountMs=6.76 · folderSwitchRecursiveNoCountMs=0.61 · indexOnlySearchMs=9.54 · ingestAssetRows=741 · ingestBytes=30424938707 · ingestFiles=20000 · ingestFilesPerSecond=271.30 · ingestIndexBuildSeconds=0 · ingestMiBPerSecond=393.60 · ingestSeconds=73.71 · ingestThumbnails=445 · mirrorTotalRows=20000 · readFloorMs=0.09 · readFloorWithJsonMs=0.10 · sameSqlTroveConnMs=0.06 · scrollTwentyWindowsMs=8.42 · sqlFloorDeepPageMs=0.25 · sqlFloorFirstPageMs=0.06 · storeReadFirstPageMs=0.16 · typedFactsFloorMs=0.07

## 进程层

| 指标 | Serpent | SerpentEmpty | Trove | TroveEmpty |
|---|---:|---:|---:|---:|
| 出窗（spawn → 合成器报告窗口） (ms) | 1543 | 1349 | 559 | 559 |
| 静止（CPU 安静 1 s） (ms) | 3046 | 2243 | 660 | 607 |
| 启动期 CPU 时间 (ms) | 2800 | 3010 | 610 | 450 |
| 进程数 | 8 | 8 | 1 | 1 |
| 线程数 | 84 | 104 | 50 | 50 |
| 常驻集 RSS (MB) | 908.80 | 1008.40 | 293.10 | 267.80 |
| 比例常驻 PSS (MB) | 572.30 | 590.60 | 257.90 | 232.40 |
| 峰值 PSS (MB) | 778.50 | 609.10 | 260.70 | 232.40 |
