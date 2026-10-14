# MODIFICATIONS.md — sodam

记录本仓库的修改类操作、验证基线与已知取舍。动代码前先读这里。

## 验证基线

改动 `crates/` 后至少通过（Linux 用 `scripts/check.sh`，带资源护栏）：

```bash
scripts/check.sh     # cargo check + fmt --check + clippy -D warnings + test
```

Windows 开发机（无 systemd 护栏，需自行限制 `CARGO_BUILD_JOBS`）：

```powershell
cargo check --offline --workspace
cargo fmt --all -- --check
cargo clippy --offline --workspace --all-targets -- -D warnings
cargo test --offline --workspace
```

**UI 改动必须真机验证**（见 `AGENTS.md`）：用 `scripts/run.sh` 启动确认，不能只过 `cargo check`。

## 已知取舍

- **暂停 / 取消后从头重下**：CDN 是否稳定支持 `Range` 续传无法保证，
  与其留下不可靠的断点，不如让状态机保持简单可预测（断点续传见 ROADMAP）。
- **并发额度现场统计**：`State::running()` 数任务而不是维护计数器。
  暂停 / 取消会在传输途中改写状态，计数器漏减一次队列就永久卡死
  （`pausing_a_running_task_frees_its_slot` 是这条的回归用例）。
- **下载与播放缓存目录分离**：`~/Music/SodaM`（下载）vs 应用 cache（播放）。
  清播放缓存不会删掉下载文件。
- **封面 / 歌词旁路是加分项**：写入失败不影响下载本身，只在 `SODAM_DOWNLOAD_LOG=1` 时打日志。
- **`Settings::fingerprint()` 不含纯界面字段**：主题 / 语言 / 快捷键 / 下载偏好
  变化不触发会话重建，否则改一次设置就要重连一次。
- **便携模式靠标记文件而不是单独编译**：同一个 exe，便携 zip 里多一个 `portable.txt`。
  好处是 CI 只构建一次、用户删掉标记文件即可退回安装版行为；代价是「便携」不是编译期
  属性，往安装目录里误放 `portable.txt` 也会切到便携布局。环境变量 `SODAM_PORTABLE=1`
  可强制开启（调试用）。
- **便携版与安装版不共享状态**：便携版把配置 / 缓存 / 下载都放在程序目录，因此登录态
  和播放缓存不会与安装版互通 —— 这是「插 U 盘就能带走」的必然代价。

## 2026-10-13 — 下载功能（Phase 1 + 2 + 3）

对齐 `sodam-download-design.md`：单曲下载、歌单 / 专辑批量下载、下载管理面板、下载设置。

### sodam-core（领域层，无 GPUI）

- **`src/download.rs`（新增）**
  - `DownloadSettings`：音质 / 目录 / 命名模板 / 封面 / 歌词 / 并发 / 重试 / 已存在策略。
  - `download_track()`：取流 → 拉流（边写边回报进度）→ 解密 → 截断校验 → `rename` →
    可选封面 / 歌词旁路。临时文件带进程号 + 序号，中断不留半成品。
  - `DownloadManager`：入队（按曲目 id 去重）、调度线程（`Weak` 持有，句柄全释放即退出）、
    每任务一个工作线程、暂停 / 继续 / 取消 / 删除 / 全部暂停 / 全部继续 / 清空已完成。
  - `DownloadFailure { retryable }`：区分网络抖动与试听 / 无权限 / 磁盘满。
  - 命名模板 `render_relative_path()`：占位符展开 + 非法字符清理 + 长度截断；
    空字段用哨兵字符精确清理（`{artist} - {title}` 在歌手为空时不会留下孤立的 `-`）。
  - 测试：13 个单元测试，覆盖命名模板、音质回退、路径解析、LRC 导出、状态机流转、去重、并发额度释放。
- **`src/config.rs`**：`Settings` 新增 `download: DownloadSettings`；
  新增 `Settings::fingerprint()`（只含网络相关字段）。
- **`src/session.rs`**：抽出 `song_for()`（播放缓存与下载共用同一份 `Song` 构造）；
  `describe_quality()` 提为 `pub(crate)`。
- **`src/lib.rs`**：导出 download 模块与类型。
- **`examples/download_probe.rs`（新增）**：完整下载链路探针（`--search` 取真实元数据）。
- **`examples/stream_probe.rs`（新增）**：打印一批曲目的取流信息，用于挑「能下整曲」的歌。

### sodam（GPUI 层）

- **`src/app_download.rs`（新增）**：入队（单曲 / 批量）、面板开关、任务控制、
  设置保存、打开文件夹（`explorer` / `open` / `xdg-open`）。
- **`src/views/downloads.rs`（新增）**：下载管理面板（进行中 / 已完成 / 失败），
  `uniform_list` 虚拟化，行内进度条 / 速度 / 剩余时间、暂停 / 继续 / 重试 / 打开文件夹。
  文案在渲染前一次算好，避免渲染闭包里重入实体更新。
- **`src/app.rs`**：`Root` 新增 `downloads` / `download_open` / `download_tab` /
  `download_cache` / `download_counts`；`DownloadTab` 枚举；挂载 `app_download` 模块与面板。
- **`src/app_actions.rs`**：心跳里 `sync_downloads()`（消费事件 + 版本号比对刷新快照）；
  `apply_settings()` 同步下载管理器设置。
- **`src/views/mod.rs`**：歌曲右键菜单加「下载」；详情页头部加「下载全部」。
- **`src/views/settings.rs`**：新增「下载设置」区块（音质 / 位置 / 命名 / 封面 / 歌词 /
  并发 / 重试 / 已存在策略）；抽出 `segmented()` 与 `toggle_switch()` 复用组件。
- **`src/ui/player_bar.rs`**：播放栏加下载入口（有任务时显示活跃数量徽章）；
  打开队列抽屉时收起下载面板。
- **`src/ui/icons.rs`** + `assets/icons/{download,folder-open}.svg`：新增两个 Lucide 图标。
- **`src/ui/i18n.rs`**：下载相关中英文案。

### 文档与版本

- `Cargo.toml` / `packaging/arch/PKGBUILD` / `README.md`：版本 `0.2.4` → `0.3.0`。
- `README.md`：功能列表加「下载」小节；配置表补 `download` 块字段说明；开发章节补探针用法。
- `docs/ARCHITECTURE.md`：分层图补 download 模块；新增「下载」架构小节。
- `docs/ROADMAP.md`：已完成项加下载；新增「7. 下载」待办（断点续传、本地优先播放等）。

### 真机验证记录（Windows x64）

- `cargo check` / `fmt --check` / `clippy -D warnings` / `test`（53 个）全通过。
- 探针：免费曲目下载 2,727,461 字节（与服务端声明一致）、`ftypM4A` 容器可被 rodio 解码；
  VIP 曲目（已登录）11,898,859 字节，封面 `.png` + `.lrc` 旁路齐全，无 `.part` 残留。
- GUI：右键下载、专辑「下载全部」、面板三个分区、暂停 / 继续、批量并发（178 首）均实测通过。
- 默认下载目录 `%USERPROFILE%\Music\SodaM` 落盘正确。
- i18n：282 个调用 key 全部有英文翻译（词典 374 条），无遗漏。

### 修复的问题（真机实测发现）

1. **暂停导致队列卡死**：并发额度原本用计数器维护，暂停正在传输的任务时
   计数器漏减，额度再也回不来 —— 剩余任务永久停在「等待中」。
   改为 `State::running()` 现场统计，并补回归测试 `pausing_a_running_task_frees_its_slot`。
2. **全部暂停后底栏按钮不切回**：`active()` 含 paused，按钮永远停在「全部暂停」。
   新增 `DownloadCounts::pending()`（queued + running）驱动按钮文案。
3. **单条继续对全局暂停无效**：`paused_all` 是全局闸门，单条继续不清它就被调度器一直跳过。
   补回归测试 `single_resume_clears_the_global_pause`。
4. **心跳每帧强制重绘**：`changed` 判断写反（`==` 应为 `!=`），面板空转也会 `cx.notify()`。
5. **渲染期重入实体更新**：下载面板原先在 `uniform_list` 闭包里 `entity.update` 取文案，
   改为渲染前一次算好 `RowLabels`，闭包只读。
6. **命名模板空字段留孤立连接符**：`{artist} - {title}` 在歌手为空时生成 `- 晴天`。
   改用哨兵字符精确清理空占位符两侧的连接符（不会误伤歌名里本来就有的连字符）。
7. **格式识别误判**：`audio/mp4` 这类 MIME 被当成未知格式落到 m4a；
   改为取 `/` 后一段再比，并补 `mpeg` / `flac` / `aac` 的兜底。

## 2026-10-13 — Windows 便携版（portable.zip）

同一个 exe，便携 zip 里多一个 `portable.txt` 标记文件；标记存在时配置 / 缓存 / 下载
全部落在程序目录，不写系统目录。仅 Windows 出便携包。

### sodam-core

- **`src/paths.rs`（新增）**：运行期目录解析的唯一入口。
  - `is_portable()`：`SODAM_PORTABLE` 环境变量优先，否则看 `current_exe()` 同目录有没有
    `PORTABLE_MARKER`（`portable.txt`）；结果用 `OnceLock` 缓存，进程内只判一次。
  - `portable_root()` / `config_dir()` / `cache_dir()` / `default_download_dir()`：
    便携时分别落在 `<root>/data`、`<root>/cache`、`<root>/down`。
  - 布局函数 `layout_*_dir()` 单独拆出来，便于对任意根目录做单元测试
    （测试进程的 exe 在 `target/debug/deps`，造不出真实便携环境）。
  - 测试：3 个（标记文件判定、便携布局、环境变量取值规则）。
- **`src/config.rs`**：`Settings::config_path()` 改走 `paths::config_dir()`。
- **`src/audio.rs`**：`cache_dir()` / `cover_cache_dir()` 改走 `paths::cache_dir()`。
- **`src/session.rs`**：4 处内联的 `dirs::cache_dir()/sodam/covers` 收敛到
  `crate::audio::cover_cache_dir()`（原先同一路径在文件里重复了 4 遍）。
- **`src/download.rs`**：`default_download_dir()` 改走 `paths::default_download_dir()`；
  默认目录测试改为便携感知（便携断言 `down`，安装版断言 `SodaM`）。
- **`src/lib.rs`**：导出 `paths` 模块。
- **`examples/paths_probe.rs`（新增）**：打印各目录解析结果，验证便携 / 安装两种布局。

### 打包与 CI

- **`packaging/windows/make-portable-zip.ps1`（新增）**：把 `sodam.exe`、`LICENSE`、
  `portable.txt`、空的 `down/` 打进 `release/sodam-<version>-windows-x64-portable.zip`，
  顶层目录 `SodaM/`，并生成同名 `.sha256`。支持 `-Version` / `-Binary` / `-OutputDir`。
  - 用 Windows 自带的 `tar.exe`（bsdtar）打包而非 `Compress-Archive`：后者写入的是
    反斜杠分隔的条目名，非 Windows 解压器会把它当成字面文件名。
  - 脚本保持纯 ASCII：Windows PowerShell 5.1 无 BOM 时按 ANSI 读 `.ps1`。
- **`.github/workflows/release.yml`**：`windows-package` 新增「Create Windows portable zip」
  步骤（复用同一次 release 构建的 exe，不额外编译），job 名 `Windows x64 installer` →
  `Windows x64`，上传步骤改名 `Upload Windows artifacts`；Release 正文补便携版说明。
  - 发布侧无需改动：`sha256sum *.zip` 与 `files: release/*.zip` 已覆盖新产物。
- **`README.md`**：平台表补便携版；配置路径表补便携例外说明；Windows 章节新增
  「便携版（portable.zip）」小节（下载 / 解压 / 目录结构 / 退回方式）；打包章节拆成
  Arch 与 Windows 便携版两节。
- **`docs/ROADMAP.md`**：已完成列表补便携版 zip。

### 真机验证记录（Windows x64）

- `cargo test -p sodam-core`：56 个通过（原 53 + `paths.rs` 3 个）。
- `paths_probe` 两种布局实测：
  - 无标记：`%APPDATA%\sodam\config.json`、`%LOCALAPPDATA%\sodam\{audio,covers}`、
    `%USERPROFILE%\Music\SodaM`（与改动前一致）。
  - exe 同级放 `portable.txt`：`<exe目录>\{data\config.json, cache\audio, cache\covers, down}`。
- zip 端到端：`make-portable-zip.ps1` 产物条目为 `SodaM/{sodam.exe, portable.txt, LICENSE, down/}`
  （正斜杠、含目录条目）；解压后运行 `SodaM\sodam.exe`，12 秒内生成 `SodaM\data\config.json`，
  同时 `%APPDATA%\sodam\config.json` 的修改时间**未变**（确认没有回写系统目录）。

