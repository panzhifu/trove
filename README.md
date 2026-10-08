<h1 align="center">Trove</h1>

<p align="center"><b>本地 · 私有 · 你自己的素材库</b></p>

<p align="center">
  <a href="./README.en.md">English</a> ·
  <a href="./LICENSE">BUSL-1.1</a>
</p>

<br>

<p align="center">
  <img src="design/main-window.png" alt="Trove 主窗口" width="900"/>
</p>

<p align="center"><i>对齐缩略图网格 · 停靠布局 · 3D 模型视口 · 视觉搜索 —— 全都跑在本地，数据永不上传</i></p>

---

## ✨ 功能特性

- **搜索** —— 全文检索 + 查询表达式（限定符 / 与或非 / 短语）+ 拼音匹配，可与标签、评分、比例等筛选自由叠加
- **视觉搜索** —— 以图搜图、按颜色找图，导入时自动算好指纹，不依赖 AI 推理
- **语义搜索** —— 按嵌入向量找内容相近的素材，本地 candle 跑 BGE 或走云端端点皆可，与全文结果混合排序
- **AI 分析** —— 多模态模型批量回写描述 / 标签 / 评分，模型新造的标签归到父标签下便于复查，整批可一键撤销
- **语音转文字** —— 音频 / 视频转写，本地 Whisper（candle）或云端端点，字幕可编辑并导出 SRT
- **组织** —— 层级标签、智能集合、嵌套收藏夹、评分与收藏
- **导入** —— 拖拽 / 粘贴 / URL / 监视文件夹 / 浏览器扩展（单张右键、拖出页面即存进那一格，或一次抓取整页媒体并标出来源与授权参考）；只链接不复制，文件留在原处
- **迁移** —— 从 Eagle / Billfish 库搬家，标签、评分、备注、来源网址与目录结构一并带走，文件仍留在原处
- **预览** —— 图片、有声视频与音频、字体实况样张、RAW / HEIC / PSD / SVG，以及 OBJ / STL / PLY / glTF / GLB / Blend 的 GPU 3D 视口（.blend 交给本机 Blender 导出）；任何一种都能 `f` 上全屏舞台，进出是动效而不是跳变
- **编辑** —— 批量旋转 / 翻转 / 裁剪、格式转换、XMP 元数据导出、截图框选直接入库
- **维护** —— 回收站、BLAKE3 完整性校验、每日自动备份、重复文件查找、多素材库
- **系统集成** —— 关闭窗口只是收进托盘、托盘菜单随时唤回（没有托盘的环境就正常退出），单实例锁保证只有一个桌面应用在写库
- **本地化** —— 9 种语言界面实时切换，默认跟随系统

## 🎯 适合谁

- **设计师 / 插画师** —— 参考图、PSD / SVG / 字体的集中素材库
- **摄影师 / 修图师** —— RAW 直读、评分与比例筛选、批量转换
- **收藏爱好者** —— 网页图片右键收藏，重复文件一键清理

## 🚀 快速开始

从 [Releases](https://github.com/panzhifu/trove/releases/latest) 下载对应平台的安装包（桌面应用与 `trove` 命令行都在里面）：

| 平台 | 下载 | 安装 |
|---|---|---|
| Debian / Ubuntu | `trove_<版本>_amd64.deb` | `sudo apt install ./trove_*_amd64.deb`（Ubuntu 24.04+ / Debian 13+） |
| Fedora / openSUSE | `trove-<版本>-1.x86_64.rpm` | `sudo dnf install ./trove-*.rpm` |
| Windows | `Trove-<版本>-Setup.exe` | 向导式安装，含开始菜单与卸载器 |
| macOS（Apple Silicon / Intel） | `Trove-<版本>-aarch64.dmg` / `Trove-<版本>-x86_64.dmg` | 拖入 Applications；未签名，首次打开**右键 →「打开」**，或 `xattr -cr /Applications/Trove.app` |
| 其他 / 便携 | `trove-<版本>-<target>.tar.gz` / `.zip` | 解压即用的裸二进制（GUI + CLI），无桌面集成 |

> 视频预览、HEIC/AVIF 解码与 PDF 缩略图依赖外部工具（`ffmpeg`、`heif-dec`、`pdftoppm`/`mutool`/`gs` 任一）。deb/rpm 已把它们列为推荐依赖；缺失时对应功能优雅降级，其余功能不受影响。

从源码构建：

```sh
git clone https://github.com/panzhifu/trove.git && cd trove
cargo run -p trove-app
```

> 需要 [Rust 工具链](https://www.rust-lang.org/tools/install)。首次启动是**欢迎界面**：给素材库起个名字就能开始，存放位置由 Trove 按系统约定管理，不必自己挑目录。

## ⌨️ 命令行

桌面应用之外还有一个无界面的 `trove` 命令，与桌面版共用同一份数据，可以在应用开着时并行使用：

```sh
trove search 猫 --aspect wechat-cover
trove import ~/Pictures --into 参考图
trove analyze --limit 50        # 视觉模型批量回写描述 / 标签 / 评分
trove doctor                    # 库、索引、ffmpeg 自检
```

共 23 个子命令；stdout 恒为一个 JSON 文档，`--help` 即完整契约。

## 🛠 开发

```sh
cargo build
cargo test -p trove-core
```

测试与 clippy 基线全绿；各模块的实现细节按主题整理在开发机的本地 `docs/` 目录，不随仓库发布。

## 📄 许可证

以 [BUSL-1.1](./LICENSE)（Business Source License 1.1）发布：

- 源码公开，可自由查看、修改、再分发，以及**非生产用途**使用；生产 / 商业用途需商业授权（联系方式见仓库）；
- 每个版本发布满四年后自动转为 [Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0)；
- 2026-09-29 起全仓库（含历史版本）以 BUSL-1.1 重新发布；此前取得的 MIT 副本仍按 MIT 执行。
