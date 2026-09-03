# Trove 开发文档

> Eagle 风格的本地资产库管理工具。
>
> 界面用 **GPUI + gpui-component**，元数据用 **libSQL**，插件系统用 **WASM（wasmtime + WIT 组件模型）**，
> 插件接口采用**静态分发 trait**（宿主侧单态化，零 `dyn`）。

## 1. 项目定位

Trove 是一个本地数字资产管理工具，对标 Eagle：

- 把散落的图片/截图/设计素材**收集进一个自包含的资产库**；
- 用文件夹、标签、评分、注释组织它们；
- 通过**插件**扩展对新文件格式（JXL、AVIF…）和批量动作（压缩、转格式）的支持；
- 界面 GPU 加速（GPUI），三栏布局：文件夹/标签树 + 素材网格 + 检查面板。

## 2. 已定技术决策

| 决策点 | 选择 | 说明 |
|---|---|---|
| 资产存储模式 | 托管库（复制进库） | 导入即复制，库自包含、可整体迁移 |
| 元数据/索引 | libSQL（本地嵌入式） | 见 [asset-repository.md](asset-repository.md)，未来可同步 Turso 云 |
| 插件 runtime | wasmtime + WIT 组件模型 | wit-bindgen 生成两侧绑定 |
| 插件分发 | 静态分发 trait | 宿主侧单态化，零 `dyn`、零 enum、运行时加载 |
| 界面 | GPUI + gpui-component | 不自己造基础组件 |
| 搜索 | v1 不做 | schema 预留，见 [asset-repository.md](asset-repository.md) §9 |

## 3. 架构分层与依赖方向

```
trove (bin)  ── main.rs：建 Application → 注册插件 → 打开库 → 渲染
   │
   ├── trove-ui           # GPUI + gpui-component 界面（三栏布局、纹理缓存）
   │
   ├── trove-plugin-host  # wasmtime 加载/实例化、注册表、分发（认识插件）
   │
   ├── trove-core         # 数据模型 + 仓库逻辑（不碰 GPUI，可 headless 测试）
   │
   └── trove-plugin-api   # host 侧 trait + WIT 生成的 host 绑定 + 共享类型
         ↑（依赖倒置的关键：插件只看得到这一层）
   plugins/*              # 官方/第三方插件，编译为 .wasm 组件，运行时加载
```

依赖方向**自上而下**，三条铁律：

1. **插件只依赖 `trove-plugin-api`**，不依赖 core / host / gpui。插件是纯函数：字节进、数据出，不碰数据库与文件系统。
2. **`trove-core` 不依赖 GPUI**。仓库产出的都是普通数据（缩略图字节、查询结果结构体），UI 层再转成 GPUI 对象。
3. **插件不链接进主程序**。插件作为独立 workspace 成员，单独构建为 `.wasm` 组件，宿主运行时加载。

## 4. 子文档索引

- **[asset-repository.md](asset-repository.md)** —— 资产仓库（核心）：身份模型、磁盘布局、libSQL schema、导入/缩略图管线、并发模型。
- **[plugin-system.md](plugin-system.md)** —— WASM 插件系统：WIT 接口、静态分发 trait 的落地、加载流程、沙箱、性能策略。

## 5. 开发环境与工具链

- Rust stable（edition 2024）。
- wasm 目标：`wasm32-wasip1`（组件构建依赖 `cargo-component`）。
- `wasmtime`：宿主运行时，版本需与 `trove-plugin-host`、`wit-bindgen` 对齐（在工作区 `Cargo.toml` 统一锁定）。
- `cargo-component`：插件侧构建工具，从 WIT 生成 guest 绑定并产出 `.wasm` 组件。
- `libsql` crate：本地嵌入式模式（`Builder::new_local`）。
- 界面：`gpui` + `gpui-component`（组件库先验证版本兼容性）。

> 注意：GPUI 与 wasmtime 的 API 仍在演进，本仓库所有模块的公共 API 设计要**把 UI 与插件 runtime 当作可替换的实现细节**，核心逻辑（core）不依赖它们。

## 6. 里程碑（Roadmap）

| 阶段 | 目标 | 交付物 |
|---|---|---|
| **M0 骨架 + spike** | 定 workspace、WIT 定义、wasmtime 加载 hello 组件、libSQL 建库读写 | 可运行的 spike；**验证 libSQL 对 FTS5 / 递归 CTE / 并发的支持度** |
| **M1 资产仓库核心** | libSQL schema、托管导入（复制 + sha256 去重）、blobs/assets、文件夹/标签增删改查、回收站 | 可 headless 测试的仓库 |
| **M2 格式插件管线** | `format.wit` 落地、WasmFormatPlugin + 注册表、缩略图生成、内置 png/jpg 原生解码、`format-jxl` 示例 | 导入任意格式并出缩略图 |
| **M3 GPUI 界面** | 三栏布局、缩略图纹理缓存、标签/评分/注释编辑、拖拽导入 | 可用的桌面应用 |
| **M4 动作插件与增强** | `action.wit`、`action-compress`、多库切换、基础搜索 | 首个可发布版本 |
