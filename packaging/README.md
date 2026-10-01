# packaging — 从已构建的二进制到安装包

这个目录里的每个脚本都**只打包、不编译**：编译发生在 `cargo build` 或
`.github/workflows/release.yml` 里，脚本吃两个参数——`<version>` 和装着
release 二进制的目录——因此本地与 CI 跑的是同一条路。

```sh
cargo build --release -p trove-app -p trove-cli
./packaging/linux/build-deb.sh 0.5.1 target/release
```

## 每个包装了什么

| 内容物 | Linux (deb/rpm) | Windows (Setup.exe) | macOS (.app/.dmg) |
|---|---|---|---|
| GUI 二进制 | `/usr/bin/trove-app` | `{app}\trove-app.exe` | `Trove.app/Contents/MacOS/trove-app` |
| CLI 二进制 | `/usr/bin/trove` | `{app}\trove.exe` | `Trove.app/Contents/MacOS/trove` |
| 桌面入口 | `trove.desktop`（原样安装） | 开始菜单 + 可选桌面图标 | bundle 自身 |
| 图标 | hicolor 16–256 六档 | `.ico` 内嵌于安装器 | `.icns` 由 PNG 组生成 |
| 许可证 | `/usr/share/doc|licenses/trove/` | — | — |

桌面包之外，release 还保留裸二进制 zip/tar.gz（两个二进制都在），供不想
装系统包的场合。二进制是**自包含**的：locale、主题、图标、着色器全部在
编译期嵌入（`include_str!`/`include_bytes!`），运行时只按 PATH 找外部工具。

## 各平台的承重约束（改动前先读）

- **`/usr/bin/trove-app` 是契约不是习惯。** `trove.desktop` 写死
  `Exec=/usr/bin/trove-app`，KWin 按 `/proc/<pid>/exe` 精确匹配这个值才
  授予 `org.kde.KWin.ScreenShot2` D-Bus 接口（进程内截图）。二进制的
  **名字**和**路径**都在契约里。这也是不打包 AppImage 的原因：挂载点路径
  不固定，会把截图授权打断。
- **glibc ≥ 2.39**（Ubuntu 24.04+ / Debian 13+ / Fedora 40+）。产物在
  ubuntu-24.04 上构建，因为 xcap → libspa 把 pipewire 头绑在 ≥ 0.3.65
  （jammy 的 0.3.48 编不过）——见 `release.yml` 的 Ubuntu 步骤注释。deb
  的 `Depends: libc6 (>= 2.39)` 和 rpm 的 `Requires: glibc >= 2.39` 都是
  这条的落点。
- **外部工具全是 Recommends/Suggests**：`ffmpeg`（视频探测/预览/音频）、
  `heif-dec`（HEIC/HEIF/AVIF）、`pdftoppm`/`mutool`/`gs` 任一（PDF
  缩略图）。缺失时对应功能降级（kind 图标、无播放），其余功能不受影响
  ——所以任何一项都不该升格成硬依赖。
- **macOS ad-hoc 签名**（`codesign --sign -`）不可省：Apple Silicon 拒绝
  执行完全未签名的二进制。它不等于 Developer ID 签名——Gatekeeper 仍会
  拦，README 写了两条放行路径。真正的签名/公证是明确不做的事，直到有
  证书（`docs/GAP-TO-SERPENT.md` §F）。
- **卸载不吃数据**。三个平台的用户数据都在 XDG/系统约定目录
  （Windows：%APPDATA%\trove、%LOCALAPPDATA%\trove，见
  `trove-core/src/paths.rs`），安装目录里没有任何运行期写入——所以
  Inno 脚本刻意没有 `[UninstallDelete]`。
- **版本号只有一个源头**：workspace `Cargo.toml` 的 `version`，发布 tag
  必须与之一致（`release.yml` 有守卫），因为应用内更新检查拿 tag 与它
  比较。tag 形状 `vX.Y.Z`。

## 本地构建

| 格式 | 命令 | 需要的工具 |
|---|---|---|
| deb | `./packaging/linux/build-deb.sh <v> <bin-dir>` | `dpkg-deb`（Debian/Ubuntu；CI 在 ubuntu-24.04 上跑） |
| rpm | `./packaging/linux/build-rpm.sh <v> <bin-dir>` | `rpmbuild`（`pacman -S rpm-tools` / `apt install rpm`） |
| dmg | `./packaging/macos/make-bundle.sh <v> <bin-dir>` | macOS 本机（iconutil/codesign/hdiutil） |
| Setup.exe | `iscc //DAppVersion=<v> //DBindir=<bin-dir> packaging/windows/trove.iss` | [Inno Setup 6](https://jrsoftware.org/isinfo.php) |

产物落在**当前目录**，命名带版本号（更新检查只看 release tag，与文件名
无关——文件名带版本是给人看的）。
