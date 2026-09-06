# Trove 前端功能差距清单

> 对照后端（`trove-core`）已具备的能力盘点前端（`trove-app`）。状态标记：
> ✅ 已实现 ｜ 🚧 本次部分实现 ｜ ⬜ 未实现
>
> 最近更新：2026-09-07

## 一、后端有但前端未暴露

| 功能 | 后端位置 | 状态 | 说明 |
|---|---|---|---|
| 资产编辑（title / description / source_url / kind） | `AssetPatch` | ✅ | Inspector 编辑区，失焦/回车保存，空值清空列 |
| 评分 rating（0–5） | `Asset.rating` / `AssetPatch.rating` | ✅ | 五角星行，点击当前最高星清除 |
| 智能集合创建 / 编辑 / 重命名 | `Library::create_smart_collection` / `rename_smart_collection` | ⬜ | 只能浏览与删除；无"保存当前搜索为智能集合" |
| 批量收藏 | `Library::set_assets_favorite`（batch.rs） | ⬜ | 收藏只对单资产 |
| 从集合移出资产 | `collections::remove_asset` | ⬜ | 只能加入 |
| 集合拖拽重排 / 改父级 | `collections::move_to` | ⬜ | 集合树固定两层、不可拖动 |
| 收藏视图 / 类型过滤 | `AssetQuery.is_favorite` / `kind` | ⬜ | 无收藏视图、无类型筛选 |
| 维护工具（缩略图重建 / FTS 重建 / 孤儿清理） | `maintenance.rs` | ⬜ | 无 UI 入口，适合放 Settings |
| 标签重命名 / 颜色 | `NewTag.color`；rename 后端也缺 | ⬜ | 需先补后端 rename |
| 整组替换标签 | `tags::set_for_asset` | ⬜ | Inspector 只逐个加/删 |
| 导出库 | — | ⬜ | 菜单项占位（disabled） |
| 通知层 | notification_layer 已挂载 | ⬜ | 导入 skipped 原因只 `eprintln!` |
| 导入进度 | `ImportPhase::Running` / `import_progress` | ⬜ | 状态存在，无面板渲染 |
| 单资产 purge / import / search facade | `Library::purge_asset` 等 | ⬜ | 前端直接调 store 层，语义等价 |

## 二、前端基础功能

| 功能 | 状态 | 说明 |
|---|---|---|
| 键盘导航：方向键移动选中 | ✅ | 网格内按行几何移动（上下取同行最近列），带滚动跟随 |
| Delete 入回收站（回收站视图为彻底删除） | ✅ | 键盘 + Edit 菜单双入口 |
| Enter 预览（大图弹窗） | ✅ | 预览主选中资产 |
| Ctrl+A 全选 / Esc 取消选中 | ✅ | 键盘 + Edit 菜单 |
| 网格虚拟化 | ✅ | `gpui::list` 变高虚拟列表，justify 行结构冻结复用 |
| 分页加载 | ✅ | 每页 200，滚动接近底部自动追加 |
| 菜单栏 File / Edit / View / Help | ✅ | `set_menus` + gpui-kit `AppMenuBar` |
| 多选工具栏 | ⬜ | 多选后无浮动批量操作栏 |
| Shift 范围选择 | ⬜ | |
| 排序 / 视图切换（网格/列表） | ⬜ | 顺序固定 |
| 拖拽集合重排 / 标签拖出 | ⬜ | 仅资产可拖 |
| 状态栏（选中数 / 库路径 / 导入状态） | ⬜ | |
| 撤销 / 重做 | ⬜ | 前后端都没有 |
| 界面多语言（中/英，实时切换） | ✅ | rust-i18n + `locales/*.toml`；设置 ▸ 语言，跟随系统 |
| 库热切换（改 path 后重开） | ⬜ | Settings 的 Browse 不回填、不重开库 |

## 三、工程性问题

- 非图片单元格图标恒为 `FileText`（`panels/common.rs` `kind_icon` 忽略参数）—— ⬜
- 大量 `eprintln!` 代替结构化日志 / 通知 —— ⬜
- 搜索输入框聚焦时全局快捷键仍注册（依赖 key context 隔离，注意回归） —— 🚧

## 建议优先级

1. 多选浮动工具栏
2. 搜索结果保存为智能集合
3. 导入进度接入通知层
4. 收藏视图 + 类型过滤
5. 维护工具入 Settings + 库热切换
