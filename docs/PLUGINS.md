# Trove 插件架构（v1 · 编译期插件）

> 写于 2026-09-19 ｜ 适用版本：0.4.5
> 回答三个问题：**插件长什么样**、**怎么写一个**、**下一步往哪长**。

---

## 0. 一句话状态

插件 = 一个实现了 `trove_core::plugins::Plugin`（core 钩子：管线阶段、命令声明）
和 `crate::plugins::AppPlugin`（UI 钩子：设置页、命令执行）的类型，在启动时
注册进两份注册表。已接线的钩子：**导入管线阶段**、**插件自己的设置页**、
**可绑定快捷键的插件命令**（设置 ▸ 快捷键 里与内置动作并列可重绑）。
随版本发布的示例插件 `sidecar-notes` 三样齐全。

---

## 1. 两份注册表

按 crate 边界拆开，依赖方向不变（`trove-core` 不认识 gpui）：

| 注册表 | 位置 | 持有的钩子 | 存储 |
|---|---|---|---|
| core | `trove_core::plugins`（`Registry` + 进程级 `register()/all()/pipeline_stages()`） | 导入管线阶段（`Stage`）、命令声明（`PluginCommand`） | `Mutex<Registry>`，进程级 |
| app | `trove-app/src/plugins/mod.rs`（`AppPlugins` Global） | 设置页（`SettingPage`）、命令执行（`run_command`） | gpui Global，与 `ThemeRegistry` 同款模式 |

一个插件是**同一个类型**实现两个 trait，`plugins::init(cx)`（`main.rs`
启动时、开窗前调用）把它同时注册进两边：

```rust
let sidecar_notes = Arc::new(builtin::SidecarNotes::new());
trove_core::plugins::register(sidecar_notes.clone());       // core：管线 + 命令声明
cx.set_global(AppPlugins { plugins: vec![sidecar_notes] }); // app：设置页 + 命令执行
```

注册契约（`default_pipeline` 与 `register_keys` 依赖它）：

- **启动时注册，第一次导入之前**——管线在首次导入时一次性快照阶段列表，
  键绑定在 `register_keys` 里读取，之后都不再读注册表；
- **插件阶段追加在内置六阶段之后**——想覆盖挖掘结果的插件因此拿到的是
  完成的元数据；
- **启用/停用（`disabled_plugins`）在构建管线那一刻读取**——开关标注了
  "下次启动生效"；插件自己的**行为设置**则由插件自持状态，改了立即生效。

## 2. 写一个插件

`crates/trove-app/src/plugins/builtin.rs`（`sidecar-notes`）是三样钩子的完整范例。

### 2.1 导入管线阶段

```rust
impl Plugin for MyPlugin {
    fn name(&self) -> &'static str { "my-plugin" }   // 持久化键，发布后不可改
    fn pipeline_stages(&self) -> Vec<Arc<dyn Stage>> { vec![Arc::new(MyStage)] }
}
```

要点：阶段失败只 skip 那一个文件；`needs/uses/produces` 声明槽位缺失会自动
补生产者；**不要**与内置阶段声明相同的 `produces`（构建会拒绝并回退内置管线）；
阶段间传自定义数据走 `io.artifacts.put/get`。

### 2.2 插件自己的设置页

实现 `AppPlugin::settings_pages()`，返回 `Vec<SettingPage>`（设置窗口每次
渲染都会重建，放心读 locale/config）。控件用 gpui-kit 现成的
`SettingField::dropdown / switch / render`。

**插件设置存哪**：`AppConfig.plugin_settings`（`插件名 → 键 → 值` 的自由区，
Trove 只负责持久化）。两种生效方式：

- **立即生效**（推荐）：插件自己持 `Arc<RwLock<状态>>`，设置页写状态 +
  顺手持久化；管线阶段每文件读共享状态。sidecar-notes 的「标题写入方式」
  就是这么做的。
- **下次启动生效**：只在构建时读配置（同启用/停用语义），页面描述里写明。

### 2.3 插件命令与快捷键

core 侧声明，app 侧执行：

```rust
impl Plugin for MyPlugin {
    fn commands(&self) -> Vec<PluginCommand> {
        vec![PluginCommand {
            action: "my-plugin/my-command",   // "插件名/命令名"，配置键 + 路由键
            key: "ctrl-alt-m",                // 默认键；空串 = 未绑定，用户可自行分配
            global: true,                     // true = 任何焦点下生效；false = 仅资产网格上下文
        }]
    }
}
impl AppPlugin for MyPlugin {
    fn run_command(&self, command: &str, window: &mut Window, cx: &mut App) { /* … */ }
}
```

机制：gpui 的 action 是静态类型，插件命令共用一个**带载荷的 action**
`RunPluginCommand { command }`（`app/actions.rs`）——`register_keys` 为每个
声明了默认键或用户覆盖的命令注册 `KeyBinding`，按键触发后经两个窗口根元素
的 `on_action` 进入 `plugins::run_command`，按 `插件名/` 前缀路由回去
（插件被停用后，残留的键绑定不再触发）。命令自动出现在 **设置 ▸ 快捷键**
的列表里，过滤、冲突标记、点击重绑全部与内置动作一致；标签按
`commands.<id 将 / 和 - 折叠为 _>` 的键找翻译，找不到就显示原始 id。

### 2.4 插件的文案与开关

- 开关不用插件写：设置 ▸ 插件 遍历注册表，每插件一行 Switch，写
  `AppConfig.disabled_plugins`。
- 文案按名查找：`plugins.<name下划线化>.name` / `.description`；命令标签
  `commands.<…>`；不提供就显示原始名。

## 3. 边界与守则

- **core 不做 i18n、不认识 gpui**——管线/命令声明在 core，设置页/命令执行在
  app，这是 `trove-plugin-api` 将来拆分的天然缝。
- 插件做后台工作必须**自开 SQLite 连接**（`Connection::open` +
  `busy_timeout`，参照 `tasks/import.rs`），`Library` 的连接是线程封闭的。
- `Plugin::name` 与 `PluginCommand::action` 都是持久化键：kebab-case，发布后不改名。
- 插件自己的可变状态用 `Arc<RwLock<…>>` 在阶段/设置页/命令间共享（`Stage`
  要求 `Send + Sync`，用 `std::sync::RwLock`）。

## 4. 下一步（按依赖顺序）

| 步骤 | 内容 | 半径 |
|---|---|---|
| API crate | 拆 `trove-plugin-api`（只含 trait 与最小类型），core/app 改为依赖它；外部 crate 从此不必链接整个 trove-core | 中 |
| 预览渲染器钩子 | `trait PreviewRenderer { fn kind(&self) -> AssetKind; … }`；`components/preview/mod.rs` 的 match 改为「先查注册表，未命中走内置」 | 小~中 |
| dock 面板钩子 | 插件返回 `BasePanel + Panel` 实体；`AppView::new` 组装 dock 后追加（必须走 `panel_handle`，见 root.rs 注释） | 小 |
| 菜单命令钩子 | 插件命令进入菜单栏（现在只有快捷键 + 将来的 UI 入口） | 中 |
| 动态加载 | `libloading` 或子进程协议（现成参照：collect HTTP 服务）；ABI 稳定后再做 | 大 |

> `docs/FEATURE-GAPS.md` 3.9-2 跟踪这项工作的剩余部分。
