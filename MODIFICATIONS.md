# MODIFICATIONS.md — sodam

记录本仓库的修改类操作、验证基线与已知取舍。动代码前先读这里。

> 未完成的工作与需注意事项记在 [`design-plan.md`](design-plan.md)；能力清单见 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

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
- **旁路固定落在下载根的子目录**（`image/` / `lrc/`），不跟随命名模板的 `/` 分层。
  好处是音乐库软件默认就会去这两处找图与词，而且换模板不会让旁路散落到各处；
  代价是模板带子目录时，不同目录下的同名曲会共用同一个 `image/<名字>.png`
  （音频本身仍各归各的目录）。
- **旁路目录按需创建**：这首歌没有封面就不建 `image/`，没有歌词就不建 `lrc/`，
  不留下空目录。
- **「本地音乐」以磁盘为准，「下载记录」以清单为准**：两者刻意分开 —— 用户手动
  放进下载目录的文件会出现在「本地音乐」（没有元数据、不给点击态），但不在
  「下载记录」里；清单只是记账，真相在磁盘上（启动时 `reconcile` 校验）。
- **`Settings::fingerprint()` 不含纯界面字段**：主题 / 语言 / 快捷键 / 下载偏好
  变化不触发会话重建，否则改一次设置就要重连一次。
- **便携模式靠标记文件而不是单独编译**：同一个 exe，便携 zip 里多一个 `portable.txt`。
  好处是 CI 只构建一次、用户删掉标记文件即可退回安装版行为；代价是「便携」不是编译期
  属性，往安装目录里误放 `portable.txt` 也会切到便携布局。环境变量 `SODAM_PORTABLE=1`
  可强制开启（调试用）。
- **便携版与安装版不共享状态**：便携版把配置 / 缓存 / 下载都放在程序目录，因此登录态
  和播放缓存不会与安装版互通 —— 这是「插 U 盘就能带走」的必然代价。

## 2026-10-13 — 下载功能（Phase 1 + 2 + 3）

对齐 `design-plan.md`（下载设计稿）：单曲下载、歌单 / 专辑批量下载、下载管理面板、下载设置。

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

## 2026-10-14 — 下载旁路改放 image/ 与 lrc/ 子目录

封面与歌词原先和音频平铺在同一目录；改为固定落在**下载根**的两个子目录：
`<下载目录>/image/`、`<下载目录>/lrc/`。音频位置不变。

### 改动

- **`crates/sodam-core/src/download.rs`**
  - 新增常量 `SIDECAR_IMAGE_DIR = "image"`、`SIDECAR_LRC_DIR = "lrc"`。
  - 新增纯函数 `sidecar_path(root, audio, subdir, extension)`：`root/<subdir>/<音频文件名>`
    再 `set_extension` 换成目标扩展名。用 `set_extension` 而非拼接，歌名里本来就有的
    点号（`A - B 1.5`）不会被截断。
  - `write_cover_sidecar` / `write_lyrics_sidecar` 改为接收下载根 `root`，各自
    `create_dir_all` 目标目录 —— 新布局比音频多一层，不能再指望音频父目录已存在。
  - 调用处（`download_track`）传入 `&dir`。
  - `DownloadSettings::write_cover` / `write_lyrics` 的字段文档同步。
  - 新增单测 `sidecar_paths_land_in_root_subdirs`：覆盖根目录落点、模板分层时旁路
    不跟随、以及含点号曲名只替换真扩展名。
- **`crates/sodam-core/examples/download_probe.rs`**：目录清单从「只列顶层」改为分别列
  `根/`、`image/`、`lrc/`，`.part` 残留断言随之覆盖三个位置。
- **`README.md`**：下载小节补目录布局图与「不跟随模板分层、按需建目录」说明；
  配置表 `write_cover` / `write_lyrics` 描述同步。
- **`MODIFICATIONS.md`**：「已知取舍」补旁路落点与按需建目录两条。

### 影响面

- **零 UI 影响**：`TrackDownloadOutcome.cover` / `.lyrics` 全仓只有 download.rs 自己写入，
  没有消费者（`app_download.rs` 零引用；下载面板用自己的网络封面缓存，
  只用 `Completed{path}` 做「打开所在文件夹」）。
- `existing_file()` 只探测音频扩展名，跳过 / 覆盖策略不受影响。
- 音质、命名模板、并发调度、状态机均未改动。

### 真机验证记录（Windows x64）

- `cargo fmt --all -- --check` 通过；`cargo test -p sodam-core` 57 个通过
  （原 56 + 新增 `sidecar_paths_land_in_root_subdirs`）。
- 真实下载（`download_probe --search 纯音乐`）落盘实测：

  ```text
  <下载目录>/dBMusic - 云，雨，风（纯音乐）.m4a          2487262 字节
  <下载目录>/image/dBMusic - 云，雨，风（纯音乐）.png    1424327 字节
  （无 lrc/ —— 该曲歌词旁路跳过，空目录未创建）
  ```

  探针退出码 0。

### 未做的事（有意）

- **不迁移旧文件**：已下载的 `.png` / `.lrc` 原地保留。`overwrite=false` 时音频已存在
  会 early-return，旁路根本不会重写；要让老歌变成新布局只能重下。
- **不做设置项**：扁平 / 子目录不可切换，直接就是子目录布局。

## 2026-10-14 — 「已下载」标记（五处落点）

在界面上标出哪些曲目已经下载到本地。仅图标（check），视觉权重低于 VIP 徽章。

### 领域层（判定 API）

- **`crates/sodam-core/src/download.rs`**
  - 新增 `pub fn scan_downloaded(settings: &DownloadSettings) -> HashSet<PathBuf>`：
    递归扫描下载目录，返回**已存在的音频文件**的相对路径（**不含扩展名**）。
    判定与内部 `existing_file()` 完全一致（扩展名属于音频集合、文件非空）。
    不跟进符号链接 / 目录联接（另有深度上限 16）；`image/`、`lrc/` 里的图片与歌词
    天然被排除；目录不存在返回空集合。
  - 新增单测 `scan_downloaded_agrees_with_the_skip_semantics`：覆盖分层目录、空文件、
    非音频、以及「`render_relative_path` 的输出能直接查这个集合」这条关键不变式。

### 状态管道（照抄 liked_ids 的形状）

- **`crates/sodam/src/app.rs`**
  - 新增 `pub struct DownloadedIndex`：把「已存在的音频相对路径集合」与
    「算这些路径时用的命名模板」**成对**快照。模板一改旧路径就对不上，两者必须同源。
    `has(&track)` = `render_relative_path` 后查集合。
  - `Root` 新增 `downloaded` / `downloaded_scanning` / `downloaded_rescan`。
- **`crates/sodam/src/app_actions.rs`**
  - 新增 `refresh_downloaded(cx)`：`cx.background_spawn` 里扫盘，回来后写快照 + `notify`。
    **合并机制**：已有扫描在跑时只置 `downloaded_rescan`，结束后补扫一次，
    避免连续触发堆出一串重复扫描。
  - `on_nav_changed` 末尾调用（覆盖「切换页面」触发）。
- **触发点**：启动时（`app.rs`）、下载完成事件（`app_download.rs` 的 `sync_downloads`）、
  下载设置变更（`apply_download_settings`）、切换页面（`on_nav_changed`）。

### 界面（五处）

- **`crates/sodam/src/views/mod.rs`**
  - 新增 `download_badge()`（照 `vip_badge()` 范式）与 `DOWNLOADED_BADGE_WIDTH`
    = `theme::ICON_SM + theme::space::SM`。
  - `track_row` 内按行从标题宽度里让出徽章占位（与 VIP 同思路，per-row）。
  - `scrolling_detail_page` 内算「已下载 N 首」并进副标题——该入口被
    我喜欢的音乐 / 歌单 / 艺人页 / 专辑页共用，改一处全生效（它的 5 个调用点无需改动）。
  - `track_menu`：已下载时「下载」→「重新下载」，保持可点。
  - 覆盖范围：歌单 / 专辑 / 艺人 / 我喜欢的音乐 / 搜索结果（共用 `track_row`）。
- **`crates/sodam/src/views/search.rs`**：`search_track_row` 持有 `root`，直接读快照，
  无需改签名；`track_list` 调用点补参数。
- **`crates/sodam/src/ui/player_bar.rs`**：队列抽屉行，标题可截断、图标不参与压缩。
- **`crates/sodam/src/views/lyrics.rs`**：播放页主标题簇，紧挨已有的 `vip_badge()`。
- **`crates/sodam/src/ui/i18n.rs`**：新增「已下载」/「已下载 {} 首」/「重新下载」三条英文。

### 真机验证记录（Windows x64）

- `fmt --check` / `clippy -D warnings` / `test`（63 个）全绿。
- i18n：词典 377 条、调用 key 283 个、**0 遗漏**。
- GUI 冒烟：启动无 panic；闲时 CPU 稳定（第二个 10s 窗口 0.47s，确认新扫描不会自旋）。
- **视觉确认未做**：本机没有桌面截图手段，徽章的实际观感需要人工过一眼。

### 已知限制

- **语义只到「下载目录里有这个文件」**：播放走的是独立的播放缓存
  （`session.rs` 的 `cached_track`），**完全不读下载目录**。所以标记不代表能离线播放、
  也不代表播放会更快。要让两者挂钩，得先实现 `docs/ROADMAP.md:72` 那条待办
  （播放优先走本地下载文件）——那是另一件事。
- **改命名模板会造成假阴性**：老文件是按旧模板落盘的，模板一改就判定不到。
- **不同曲目渲染同名会互相冒充**（假阳性），与下载侧「已存在即跳过」的固有局限同源。

## 2026-10-14 — 修复：下载标记把 VIP 徽章推歪（用户实测反馈）

### 问题

列表里「VIP + 已下载」的行，VIP 徽章比「仅 VIP」的行**左移 22px**，各行对不齐。

### 根因

标题列宽原先**按行**从 `cols.title` 里减：

```rust
if track.vip { text_width -= VIP_WIDTH; }              // 76
if is_downloaded { text_width -= DOWNLOADED_BADGE_WIDTH; }  // 22
```

VIP 徽章紧跟标题之后，标题一变窄，VIP 的 x 就跟着移动。

### 修法（`crates/sodam/src/views/mod.rs`）

改成**交给 flex 分配**：

```text
[标题 flex_1] [VIP?] [勾选槽位 —— 宽度固定、永远渲染]
```

关键在**勾选槽位无条件存在**（未下载时是空 div）。槽位一旦按行出现/消失，VIP 就会被推着走。
这样 VIP 与勾选的 x 在所有行都一致；代价是无徽章的行标题少约 22px。

- 删掉 `VIP_WIDTH`（手工减宽度的做法不再需要，flex 用徽章的真实宽度）；
  改为 `DOWNLOADED_SLOT_WIDTH = theme::ICON_SM`。
- `download_badge()` 简化为纯图标（宽度由槽位负责）。
- 上一版引入的按行减宽逻辑（`DOWNLOADED_BADGE_WIDTH`）随之删除。

## 2026-10-14 — 本地音乐页（M1）

在第 5 轮方案的基础上落地 M1：把「下载完成后重启就丢」这件事解决掉。

### 领域层

- **`crates/sodam-core/src/download_library.rs`（新增）**：下载清单的读写与校验。
  - `LibraryEntry`：**主键是 `relative`（相对下载目录的路径、不含扩展名）而不是 `track_id`** ——
    为后续「扫描任意本地文件夹」预留（扫来的文件没有汽水 id）。元数据（title/artist/album/
    cover/duration/track_id… ）全部 `Option`，另留 `source`（Downloaded / Scanned）与 `mtime`。
  - `load` / `save`（临时文件 + `sync_all` + rename 原子写）/ `upsert`（按 `relative` 去重，
    覆盖时保留原 `added_at`）/ `remove` / `reconcile` + `reconcile_with`。
  - 清单文件 `<下载目录>/.sodam-downloads.json`，带 `version` 字段；**缺失 / 损坏 / 版本不认识
    一律返回空集合**，不让记账文件把页面或启动搞挂。
  - `upsert`/`remove` 用一把 `static Mutex` 串行化读改写，避免并发任务互相覆盖。
  - 12 个单元测试：往返、去重与 `added_at` 保留、remove 命中/未命中、损坏与未知版本退化、
    reconcile 分层目录与旁路排除、`to_track` 缺字段拒绝、展示回退。
- **`crates/sodam-core/src/download.rs`**
  - `finish_task` 接收 `&DownloadSettings`，在**状态锁之外**落一条清单
    （占着任务锁做 I/O 会拖住调度器与 UI）。
  - 「已存在（跳过）」也落账 —— 老版本留下的文件借此补进清单；但那种情况质量未知，
    记 `None` 而不是把「已存在」当成音质写进清单。
  - 新增集成测试 `finishing_a_download_writes_the_library_manifest`：直接驱动 `finish_task`，
    断言清单落盘、能还原成 `TrackItem`、重新读 + 校验后仍在，以及跳过项的 `quality == None`。
  - `lib.rs` 导出 `download_library`。

### 界面

- **`crates/sodam/src/app.rs`**：`Nav` 新增 `Local`；`nav_display_label` 加「本地音乐」；
  `Root` 新增 `library` / `library_missing` / `local_scanning` / `local_rescan`；
  启动调用 `refresh_local`；`SODAM_NAV` 增加 `local`（开发期直达该页）。
- **`crates/sodam/src/app_actions.rs`**：`refresh_downloaded` → **`refresh_local`**：
  一次扫盘同时刷新「已下载」标记与本地音乐库（省一半 I/O），仍用「已有扫描只记待重扫」的合并策略。
- **`crates/sodam/src/ui/sidebar.rs`**：导航项加进 `library_items`（图标 `disc-3`）。
  **登录豁免**：原逻辑是「未登录时除设置页一律弹回」；本地音乐是纯本地内容，
  一并放行（`!matches!(nav, Nav::Settings | Nav::Local)`），否则未登录根本点不进去。
- **`crates/sodam/src/views/local_music.rs`（新增）**：横向三段式入口
  （`LocalTab::{Songs, Downloads, Scan}`，见下方「M1 定稿」）。
  - **本地音乐**：**以磁盘为准**（`Root.local_files`）；行显示 封面 / 歌名 / 歌手 / 时长 /
    大小 / 音质，点击从该行开始播放。
  - **下载管理**：**以记录为准**，内含 `DownloadTab::{Active, Failed, Completed}` 三个子 tab
    （下载中 / 下载失败 / 已下载）。
  - **扫描本地音乐**：占位入口（能力见 `design-plan.md` 的 B 阶段）。
  - 缺 `track_id` 的条目（后续扫描来的）还原不出 `TrackItem`，**不给点击态**，不伪装可点。
- **`crates/sodam/src/views/mod.rs`**：`subtitles_for` 与 `render` 补 `Nav::Local`；
  `downloads::human_bytes` / `row_labels` / `task_row` / `RowLabels` 提为 `pub(crate)` 以便复用。
- **`crates/sodam/src/ui/i18n.rs`**：新增本地音乐相关文案（本地音乐 / 下载管理 / 下载中 /
  待重下 / 未知音质 / 清理记录 / 各空态 / 标题栏汇总等）。

### M1 定稿（含 18:52 记录之后的演进）

首次落地的「三分区单列表」在当天最后一轮被重构为**三段式 tab**，并补了两个能力。
以下为当前代码的实际状态：

- **`app.rs`**：新增 `LocalTab` 枚举（`Songs` / `Downloads` / `Scan`，默认 `Songs`）与
  `Root.local_tab`、`Root.local_files: Arc<Vec<LocalFile>>`；`LocalFile` 是「磁盘上真实存在的
  一个音频文件」，`entry: Option<LibraryEntry>` 挂清单元数据（没有则 `None`）。
- **`download.rs`**：新增 `ScannedFile { relative, extension, bytes, modified }` 与
  `scan_local_files()`（完整版，按 mtime 倒序，mtime 取不到时按相对路径，顺序稳定）；
  `scan_downloaded()` 降级为它的轻量包装（只取相对路径）。`SCAN_MAX_DEPTH = 16`、
  不跟进符号链接 / 目录联接的规则两者共用同一套 `walk_audio`。
- **`app_actions.rs`**：`refresh_local` 一次扫盘喂**三样**东西 —— 「已下载」标记、
  清单校验（`reconcile_with`）、本地音乐列表（`LocalFile`）。
- **`app_download.rs`**：新增 `clear_download_records`（「清理记录」）——
  清掉失败 / 已取消的任务 + 清单里已失效（磁盘上已没有）的条目，**一律不碰磁盘文件**。
- **`local_music.rs`**：`downloads_view` 复用 `settings::segmented` 做子 tab；
  `records_view`（已下载）来自清单、不依赖内存任务；`failed_view` 把
  「失败 / 已取消任务」与「幽灵条目」混排为 `FailedRow`。

### 真机验证记录（Windows x64）

- `fmt --check` / `clippy -D warnings` / `test`（75 个）全绿（此为 M1 首次落地时的记录；
  18:52 之后的演进轮次的复验见下节）。
- i18n：词典 385 条、调用 key 289 个、**0 遗漏**。
- GUI：`SODAM_NAV=local` 直达该页，启动与渲染**无 panic**、stderr 干净；
  稳定后 CPU 0.41s/10s，与改动前基线（0.47s/10s）一致 —— 新页面不引起自旋。
- 集成测试锁住了「下载完成 → 清单落盘 → 重读 + 校验仍在」这条命门。

### 本次未做（属于后续阶段）

- **M2**：行内菜单、两个删除语义（从列表移除 / 删除本地文件 + 二次确认）、播放全部。
- **M3**：专辑 / 歌手 / 文件夹分组与排序；**页内搜索按决定暂缓**（要动 IME 路由，单独立项）。
- **视觉确认未做**：本机没有桌面截图手段，三段式入口与各列宽需要人工过一眼。
- 抽屉与页面并存的职责划分（抽屉收窄为「当前任务」）也留到 M2。

## 2026-10-14 — 记录校验与修复（工具链复验）

对工作区做了一次「代码 ↔ 记录」一致性校验，发现记录滞后于最后一轮改动，并修掉了
当时未过的提交门。

### 修复

- **`crates/sodam-core/src/download.rs:1852`**：`clippy::cmp_owned` ——
  `file.relative == PathBuf::from("A")` 改为 `file.relative.as_path() == Path::new("A")`
  （测试内，`-D warnings` 下会编译失败）。
- **`cargo fmt --all`**：4 处格式漂移（`app_actions.rs` 的 import 折行、`app_download.rs` 的
  `if let Err` 换行、`local_music.rs` 两处调用折行）。

### 复验（Windows x64，`CARGO_BUILD_JOBS=6`）

- `cargo fmt --all -- --check`：通过。
- `cargo clippy --offline --workspace --all-targets -- -D warnings`：通过。
- `cargo test --offline --workspace`：**76** 个通过（`sodam-core` 71 + `sodam` 5）。
- i18n：词典唯一 key **395** 条、调用 key **0 遗漏**（宽口径 276 / 窄口径 229，
  两个口径结论一致）。

### 记录更正

- 上文各节的「测试 75 个」是当时值；当前工作区为 76（新增 `scan_local_files` 系列测试）。
- 「三分区单列表」的描述已过时，实际为三段式 `LocalTab`（见「M1 定稿」）。
- 测试数、i18n 计数的口径以本节为准（此前几节的数字是各轮当时值，不再逐条回改）。

## 2026-10-14 — 修复：本地音乐列表封面不显示（用户实测反馈）

### 问题

「本地音乐」与「已下载」两个列表的封面**全部是占位块**（音符图标），有封面的曲目也不显示。

### 根因

`crates/sodam/src/views/local_music.rs` 的两处封面解析把 **URL 字符串当成文件路径**：

```rust
.and_then(|url| covers.get(url).map(|_| url.to_string()))   // ← 返回 URL，不是缓存路径
```

`Root.covers` 的语义是 `URL → 本地缓存文件路径`，而 `artwork::cover(path, …)` 会做
`path.exists()` 判定。传进去的是 `https://…` 字符串，`exists()` 永远为假 → 永远走占位块。

第二个问题是这两处**从不把缺图 URL 排进 `cover_requests`**，所以即使渲染了也永远
不会去取封面（其它列表都靠这个队列触发下载）。

### 修法

- 新增 `resolve_cover(url, covers, cover_requests) -> Option<PathBuf>`：命中缓存返回
  本地路径；未命中且 URL 非空时排进请求队列（带去重）。语义与 `mod.rs` / `scenes.rs` /
  `player_bar.rs` 的既有写法一致。
- `row_cover` 签名从 `Option<&str>`（URL）改为 `Option<PathBuf>`（缓存路径），
  让类型本身拦住「拿 URL 当路径」这类错误。
- 两处调用点（`songs_view` / `records_view`）改用 `resolve_cover`。
- 新增 3 个回归测试：`cached_cover_returns_the_local_path_not_the_url`、
  `missing_cover_queues_the_url_once`、`blank_cover_is_ignored`。

### 影响面

- 只改 `local_music.rs`；其它列表（`mod.rs` / `search.rs` / `scenes.rs` / `player_bar.rs` /
  `downloads.rs`）的封面逻辑未动，写法本来正确。
- 「下载中」与「下载失败」两个 tab 本来就没有封面列（设计如此），不涉及。

### 复验（Windows x64）

- `fmt --check` / `clippy -D warnings` / `test` 全绿；测试总数 76 → **79**。

### 真机复验与「跑的是旧二进制」的教训

用户第二次反馈「还是不显示」时，实测发现：**跑的是 19:44 的旧二进制，而修复在 22:06**
—— 上一轮只跑了 `cargo check` / `test`，没有重新 `cargo build`，所以 `target/debug/sodam.exe`
仍是修复前的版本。

重新构建（22:15）后真机验证（`SODAM_THEME_LOG=1` 抓封面链路 + 截图）：

- 日志确认两首曲目的封面 URL 被取到并命中缓存：
  `[theme] cover url="…" path=Some("1732ce0a51433497-160.img") color=Some((101, 127, 141))`
  —— 修复前这两条 URL **从不进入请求队列**，日志里不会有它们。
- 截图确认：「本地音乐」列表三首有元数据的曲目（黑塔 / 两不知 / 半故）封面正常；
  两个手动放进下载目录的文件（无元数据）按设计走占位块。
- 「下载管理 → 已下载」三个记录同样正常显示封面。
- 主列表（我喜欢的音乐 / 我的歌单）封面本就正常，未受影响。

**教训**：改完 UI 代码后必须重新构建二进制再真机验证，`cargo check` / `cargo test`
不会更新 `target/debug/sodam.exe`（`cargo test` 只构建测试 target）。
按 `AGENTS.md` 第 6 条，UI 改动要真机验证 —— 这一步不可省。

## 2026-10-14 — 本地音乐可播放（含无元数据文件）+ 播放页下载入口

用户实测反馈两件事，都改掉了。

### 问题 1：本地音乐列表里「点了没反应」

**实测**（`SODAM_PLAYER_LOG=1`）：有清单记录的行能播（`[player] 开始装载：黑塔` → `装载完成`），
但**用户手动放进下载目录的文件（无元数据）完全无响应** —— 日志零输出。

**根因**：`songs_view` 只给 `entry.to_track()` 成功的行挂 `on_click`，
没有汽水 `track_id` 的文件被判定为「不可播放」。

**更深的问题**：即使可点，点击后走的是 `play_from_arc` → `start_track` →
`session.download_to_cache()` —— **文件就在磁盘上，却要重新联网拉流**。

**修法**：

- **`crates/sodam/src/app.rs`**
  - `LocalFile` 新增 `extension: String`（扫盘时 `ScannedFile` 有，原先构造 `LocalFile` 时丢掉了；
    拼绝对路径需要它）。
  - 新增 `LocalFile::absolute_path(settings)`：`<下载目录>/<relative>.<extension>`。
    **字面拼接扩展名，不能用 `with_extension`** —— 它会把曲名里本来就有的点号当成扩展名
    截断（`A - B 1.5` → `A - B 1.m4a`）。这条被下面的测试抓到，是真实踩过的坑。
  - 新增 `LocalFile::to_track()`：有清单记录用记录的元数据；没有则合成
    `id = "local:<relative>"`（**必须非空** —— `toggle_play` 等处以 `snapshot.track_id.is_empty()`
    判断「引擎还没装载」）、标题退化成文件名。
- **`crates/sodam/src/app_actions.rs`**
  - 新增 `play_local_file(files, index, cx)`：整表入队 + 直接 `engine.load` 磁盘文件，
    **不联网**；`queue_origin = QueueOrigin::Local`。
  - 新增 `local_playback_path(&track)` / `local_quality_for(&track)`：本地曲目 → 磁盘路径 / 音质。
  - `start_track` 开头识别本地曲目：直接装载本地文件（否则**自动下一首**会对合成 id
    发起网络拉流，连续失败 3 次后暂停）。
  - `spawn_prefetch` 跳过本地曲目（合成 id 打网络只会白耗签名服务）。
- **`crates/sodam/src/app.rs`**：`QueueOrigin` 新增 `Local` 变体。
- **`crates/sodam/src/views/local_music.rs`**：`songs_view` 与 `records_view` 的**每一行都可点**
  （去掉了「只有还原得出 TrackItem 才挂 on_click」的限制），统一走 `play_local_file`；
  删除随之失效的 `playable_from` 辅助函数。
- 新增 3 个单元测试：合成 id 非空 + 标题回退、清单元数据优先、绝对路径拼接
  （含点号曲名、含模板分层目录两个用例）。

### 问题 2：播放页只有「已下载 ✓」，未下载时没有入口

**修法**（`crates/sodam/src/views/mod.rs` + `views/lyrics.rs`）：

- 新增 `download_slot(track, is_downloaded, cx)`：已下载 → 勾选图标（不可点）；
  未下载 → 下载图标，**可点，点击入队下载**（`open_when_done = false`）。
- 播放页曲目簇里 `.when(is_downloaded, ...)` 换成 `.child(download_slot(...))` ——
  **槽位永远存在**，与列表行的固定槽位同理，标题 / VIP / 收藏的相对位置不会因下载状态跳动。
- 下载完成 → `sync_downloads` 里的 `refresh_local` 会让 ✓ 自动出现，无需额外接线。

### 影响面

- 本地播放只覆盖「本地音乐」与「已下载」两个列表；其它列表（歌单 / 专辑 / 搜索 /
  队列抽屉）仍走网络取流，未改动。
- 「已下载」判定语义未变（仍是「磁盘上有这个文件」）；本次只让它**真的能播**。
- 本地曲目没有汽水 id，所以不加载歌词、不发起封面网络请求（封面走清单里的 URL 缓存）。

### 真机实测发现并一并修掉的连锁问题

本地曲目（合成 id `local:<relative>`）在汽水没有条目，凡是用 `track.id` 打网络的路径
都会失败或报错。逐个堵掉：

1. **自动下一首会拉流失败**：`start_track` 原先无条件走 `download_to_cache`。
   本地曲目播完 → 自动切下一首 → 拿合成 id 请求取流 → 连续失败 3 次后暂停。
   修法：`start_track` 开头识别本地曲目，直接装载磁盘文件（`local_playback_path`）。
2. **预取白打网络**：`spawn_prefetch` 会对本地曲目发起预取。修法：跳过本地曲目。
3. **播放页报「获取歌词失败」**：`ensure_playback_assets` / `open_lyrics` / `start_track` /
   `retry_playback_assets` 四处都会加载歌词，实测报
   `soda track v2 parse failed ... ERR_INVALID_PARAM`。修法：这四处都跳过本地曲目。
4. **播放上报打空**：心跳里的 `play_reporter.observe` 会把合成 id 当汽水曲目上报
   （写「最近播放」+ 埋点）。修法：`local:` 前缀的曲目跳过上报。
5. **播放页会显示可点的「下载」按钮**：本地文件按定义就在磁盘上，点下载只会拿合成 id
   去请求。修法：`is_downloaded` 对本地曲目恒为真（显示 ✓，不给下载按钮）。
6. **播放页挂着上一首的歌词**：本地曲目跳过歌词加载后，`Root.lyrics` 仍是上一首的内容，
   会显示在新曲目下面（真机截图发现）。修法：新增 `clear_lyrics_state()`，
   `play_local_file` 与 `start_track` 的本地分支都调用它（顺带把在途旧请求作废）。

为此抽出 `is_local_track()`（`app_actions.rs`，`local:` 前缀判定）+ `Root::is_local_track()`。
真实汽水 id 是 19 位纯数字，不会撞这个前缀。

## 2026-10-15 — 本地时长 / 旁路歌词封面 / 元数据匹配

三件事一起做（用户要求一次性完成）：时长探测、读旁路 `image/` + `lrc/`、
以及给没有元数据的文件联网匹配信息。

### Phase 1 — 时长

**背景**：`Yang.m4a` 这类用户手动放进下载目录的文件，`track.duration_seconds` 是 0，
导致播放页显示 `--:--`、进度条分母是 0（用户报的 bug）。

- **`crates/sodam-core/src/audio.rs`**
  - `Command::Load`：decoder 建好后读 `total_duration()`（只读容器头，不解码），
    **探测值优先**、取不到才回退 `track.duration_seconds`。
  - 新增 `pub fn probe_duration(path) -> Option<f64>`：MP4 系走**手写 mvhd 解析**
    （`probe_mp4_duration`，只读文件头部 ≤2MB，微秒级），其它格式交给 rodio。
  - 测试：`probe_mp4_duration_reads_the_mvhd`（v0 + v1 两种头）、
    `probe_mp4_duration_rejects_non_mp4`。
- **`crates/sodam-core/src/download.rs`**
  - `ScannedFile` 新增 `duration_seconds: Option<f64>`（`Eq` 派生随之去掉 —— `f64` 不是 `Eq`）。
  - `scan_local_files` 新增 `known_durations` 参数：**清单里已有的不重复读盘**；
    新增 `scan_local_files_fast`（不探测）供「已下载」标记等只看存在性的调用方用。
- **`crates/sodam/src/app.rs`**：`LocalFile` 新增 `probed_duration`，
  `duration_seconds()` = 清单权威值 → 探测值 → 无；`to_track()` 用它填 `duration_seconds`。
- 列表时长列改用 `LocalFile::duration_seconds()`。

### Phase 2 — 旁路封面 / 歌词

**背景**：下载时写的 `<下载目录>/image/*.png` 与 `<下载目录>/lrc/*.lrc` **一直没人读**。

- **`crates/sodam-core/src/download.rs`**
  - 新增 `ScannedSidecars { covers, lyrics }` 与 `scan_sidecars(settings)`：
    两个 `read_dir`（旁路是扁平的），键用**文件名 stem**。
  - 封面候选扩展名 `png/jpg/jpeg/webp`（写侧用魔数嗅探，扩展名不固定）。
  - 新增 `read_sidecar_lyrics(path)`：读文件 + 复用现有 `parse_lrc`。
- **`crates/sodam-core/src/session.rs`**
  - 新增 `file://` 伪 URL 支持：`cover_path` 识别后走 `cover_path_from_local`
    （**跳过下载**，直接读盘 + 缩放到 160px 缓存），缓存键是伪 URL 本身。
  - `original_cover_path` 拆成两层：`file://` → 直接返回原文件（旁路图本就是 960px 级）；
    网络 URL → 原 `remote_original_cover_path`。
  - 这样**列表与播放页的渲染路径完全不用改** —— 它们只认 `Root.covers`。
- **`crates/sodam/src/app_actions.rs`**：`refresh_local` 一次扫盘喂**四样**东西
  （标记 / 清单校验 / 本地列表 / 旁路），`LocalFile` 带上 `cover_path` / `lyrics_path`。
- **`crates/sodam/src/views/local_music.rs`**
  - 新增 `resolve_local_cover`：**旁路优先** → 清单里的网络 URL → 占位块。
  - `records_view`（已下载）同样走旁路。
- **`crates/sodam/src/views/lyrics.rs`**：播放页封面优先用旁路图（`file://` 伪 URL）。
- **`crates/sodam/src/app_actions.rs`**：新增 `load_local_lyrics` —— 本地曲目查旁路 `.lrc`
  并解析；`play_local_file` / `start_track` 本地分支 / `ensure_playback_assets` 都走它。

### Phase 3 — 元数据匹配

**背景**：清单丢失 / 从别的机器迁移 / 用户手动放的文件，只有文件名。

- **`crates/sodam-core/src/match_metadata.rs`（新增）**
  - `guess_artist_title(stem)`：按 `" - "` **从右往左**拆（歌名里更可能有连字符）。
  - `score_candidate`：标题归一化全等 +50 / 包含 +30；歌手命中 +20；
    **时长 ±2s +30、±5s +10**（最强的防误判信号）。**标题对不上直接 0 分**
    —— 防「歌手对 + 时长对」把错歌凑过线。
  - 阈值 `MATCH_THRESHOLD = 60`：宁可漏，不可错。
  - `match_track(session, stem, duration_hint)`：搜索 → 打分 → 取最高。
  - 6 个单元测试：拆分、全等+时长、**标题不符必须 0 分**、未知时长不给分、
    `(Live)` 归一化、松匹配只给部分分。
- **`crates/sodam/src/app_actions.rs`**
  - `match_local_file(relative, cx)`：单条匹配（右键菜单用）。
  - `match_all_local_files(cx)` + `run_match_batch`：批量，**串行 + 每首间隔 350ms**
    （避免打爆签名服务），每完成一首刷新状态；网络类失败**中断整批**。
  - `apply_match`：写回清单（`source = Scanned`，保留原音质 / 入库时间；
    音质未知就留空，不冒充）。
- **`crates/sodam/src/views/local_music.rs`**
  - 新增行右键菜单 `local_menu`（目前只有「匹配信息」；已有 id 或未登录时置灰）。
  - `songs_toolbar`：待匹配数量 + 「补全信息（N 首）」按钮 + 匹配进度显示。
- **`crates/sodam/src/app.rs`**：`Root` 新增 `matching` / `local_menu`；
  `local_menu` 接入渲染树与遮罩关闭逻辑。
- i18n 新增 11 条（匹配信息 / 补全信息 / 匹配中 / 完成 / 未找到 / 失败等）。

### 已知取舍

- **旁路按 stem 匹配**：不同目录的同名曲会共用同一张 sidecar（既有设计的必然，
  与「已存在即跳过」同源）。
- **匹配只写清单，不动音频文件**：这是 (a) 方案的边界 —— 文件本身仍没有内嵌标签，
  拷到别的播放器看不到信息（要那样得走 `lofty`，见 ROADMAP）。
- **匹配不自动触发**：迁移 500 首时自动匹配会打爆接口；必须用户点按钮。

## 2026-10-15 — 修复：播放页重试加载 / 本地封面 / 详情页循环

用户实测反馈三个问题，根因都在「同一份数据有两个来源」这类不一致上。

### 问题 1：匹配成功后播放页仍显示「重试加载」

**根因**：`ensure_playback_assets` 用 `track.cover`（**网络 URL**）准备封面，
而 `lyrics_view` 渲染时用旁路 `file://` 伪 URL 取图 —— 两者对不上，
`original_covers.contains_key(伪 URL)` 永远为假 → 界面一直显示重试按钮。

本地曲目（`Yang`）的 `track.cover` 本来就是**空的**，问题更明显。

**修法**：抽出 `Root::playback_cover_source(&track)` 作为**唯一入口**
（本地曲目 → 旁路 `file://`；否则 → `track.cover`），四处调用点统一：
`ensure_playback_assets` / `lyrics_view` 渲染 / `retry_playback_assets` /
`play_local_file` + `start_track` 的本地分支。

### 问题 2：下载入口有图，播放页不显示封面

同一个根因：`play_local_file` 与 `start_track` 的本地分支**从未准备封面**
（只有 `ensure_playback_assets` 在进播放页时才准备，且用的还是错的 URL）。
修法同上 —— 两处都改为用 `playback_cover_source` 准备。

### 问题 3：专辑页点歌名会再次进入同一专辑页（循环）

**根因**：专辑页的每一行也走 `track_row` → `open_track_album`。
在专辑 A 的页面里点任一曲目的歌名，会**再次**打开专辑 A —— 返回栈被同一个
专辑塞满，看起来像死循环。

**修法**（`app_search.rs`）：
- `open_track_album` / `open_track_artist` 加「已经在这个详情页就不重复打开」判断
  （比较 `open_album.album.id` / `open_artist.artist.id`）。
- `set_nav` 补一条防护：**栈顶与当前页相同就不重复压栈**
  （详情页反复进出时栈里会攒一串同名项，返回要按很多次）。

### 验证

- `fmt --check` / `clippy -D warnings` / `test` / `build` 全绿。
- 新增回归测试 `local_cover_source_prefers_the_sidecar`：
  断言无元数据文件的封面来源是 `file://` 伪 URL 且**非空**。

### 补修：底部播放条与队列抽屉的封面

上一轮漏了 `player_bar.rs` 两处 —— 它们仍在用 `track.cover` 直接查缓存：

- **底部播放条**（`player_bar.rs:292`）：`root.cover_of(&track.cover)`。
  本地曲目的 `track.cover` 是空的，所以播放条封面一直是占位块。
  修法：改用 `root.cover_of(&root.playback_cover_source(track))`。
- **队列抽屉**（`player_bar.rs:756`）：渲染闭包拿不到 `&Root`，改为在闭包外
  预先算好 `local_covers: HashMap<track_id, source>` 传进去。

**教训**：「同一份数据有多个消费点」时，抽了唯一入口还不够 ——
要把**所有**消费点都切过去。这次是靠用户指出播放条才发现的。

## 2026-10-15 — 推荐预载 / 手动选择匹配 / 右侧面板改为叠加

用户提的三件事，都先确认现状再改。

### 1. 点「推荐」不再临时加载

**确认**：启动时**已经**调了 `start_recommendation(false)`（`app.rs`），
但 `on_nav_changed` 的 `Nav::Home` 分支**每次都再拉一遍网络** ——
用户点侧边栏看到的就是「正在加载推荐队列…」。

**修法**（`app_actions.rs`）：新增 `enter_recommendation()`：
- 队列已经是推荐来源（`queue_origin == Feed` 且队列非空）→ **直接打开播放页，不发请求**
- 否则才 `start_recommendation(true)`

`on_nav_changed` 的 `Nav::Home` 分支改用它。
（`activate_recommendation` 本来就有这个判断，但那是另一个入口 —— 侧边栏走的是
`set_nav` → `on_nav_changed`，没被覆盖到。）

### 2. 匹配失败支持手动选择

**领域层**（`match_metadata.rs`）：
- 新增 `search_candidates()`：返回**按分数降序**的前 `MAX_CANDIDATES = 5` 条
  （**不过阈值** —— 用户可能一眼认出冷门翻唱，自动分不够但确实是对的）。
- `match_track()` 改为复用它再筛阈值，行为不变。

**UI**（`local_music.rs` + `app_actions.rs`）：
- 新增「手动选择匹配」浮层 `match_picker`：候选列表显示**排名（1/2/3…）+ 封面 +
  歌名 + 歌手 + 时长**，点击即应用。
  - **只显示排名不显示分数**：内部权重对用户没有意义，还会暴露实现细节。
  - 顶部显示目标文件名，避免选错文件。
- 右键菜单新增「手动选择匹配」入口（与「匹配信息」并列）。
- `match_local_file` 改为：够阈值直接应用；**有候选但不够阈值时自动打开浮层**；
  候选为空才提示「未找到匹配」。
- 新增 `open_match_picker()`（直接拉候选，不尝试自动应用）与
  `apply_picked_match()`（写回清单，分数记 0 表示"用户指定"）。
- `Root` 新增 `match_picker: Option<(PathBuf, Vec<MatchOutcome>)>`。

### 3. 右侧面板改为**叠加**（不再挤压内容）

**确认**：主界面的队列抽屉（320px）与下载面板（360px）是 flex **兄弟节点**，
打开就把内容区压窄 —— 而列表列宽是 `columns_for` 按可用宽度算的，
一压就错位。播放页不受影响（封面 + 歌词自适应，本就有缩放兜底）。

**修法**（`app.rs`）：两个面板外层包一层 `absolute top/right h_full`，
**浮在内容之上**；内容区宽度保持不变。

**宽度必须按播放页的做法在 Rust 侧算**（`theme::overlay_panel_width`）：

- 第一版写成 `max_w(relative(0.40))` 结果**只显示一半** —— 叠加层是
  `width: auto` 的绝对定位盒子，GPUI 把 `relative(0.40)` 按**元素自身宽度**
  解析（320px × 0.4 = 128px），把面板压成一条。真机截图里抽屉右半边是空的。
- 正确做法（与播放页 `lyrics.rs` 的 `lyrics_width` 同一套）：视口宽度
  `window.viewport_size()` 进 Rust，算成确定像素再传给面板 ——
  `overlay_panel_width(viewport, design) = min(design, viewport − 侧边栏 − 320)`
  再兜底 `max(240)`（比官方最小窗口还窄时给内容区保底的那 320px 先让位）。
- 面板函数改为接收 `width: f32` 参数（`queue_drawer` / `downloads::panel`），
  `queue_panel`（播放页内嵌）改用 `Option<f32> = None` 表示「宽度由父容器决定」。
- 加 `shadow_lg()`：叠加后需要视觉层次，否则和下面内容糊在一起。

**保持不动的**：播放页的队列面板仍是挤压式（`right_module` + `lyrics_width`）——
它是自适应布局，本来就没有列错位问题。

### 验证

- `fmt --check` / `clippy -D warnings` / `test` / `build` 全绿（96 个测试）。
- 新增测试 `candidates_sort_descending_and_truncate`（排序 + 截断）与
  `overlay_panel_tests`（宽/最小/窄视口三档宽度）。
- i18n 零遗漏（新增「手动选择匹配」「选择匹配」「有 N 个相似结果」等）。
- **真机复验（2026-10-15，`SODAM_POPUP=queue|downloads` 截图）**：
  窗口内容区 ~1188px 宽下，队列抽屉左边缘 x≈868（= 1188 − 320），
  行内标题 / 歌手 / 时长 / 红心全部排得下；下载面板左边缘 x=828
  （= 1188 − 360），三个分段标签、空态图标与底栏按钮全部可见。
  **修复前**抽屉只有 128px（320 × 0.40），行内只剩封面和时长。
- `SODAM_POPUP` 顺带扩为 `queue|downloads`，便于两个面板分别截图核对。

## 2026-10-15 — 修复：本地菜单 / 匹配浮层缺 `with_priority`（点击被遮罩吞掉）

**发现**（「手动选择匹配」真机复验时）：`views/local_music.rs` 的 `local_menu` 与
`match_picker` 是全仓库仅有的两个**没写 `.with_priority()`** 的 `gpui::deferred`
（默认 0）—— 菜单遮罩是 1、队列 / 轨道菜单是 2。

GPUI 的 deferred 按**优先级升序** prepaint / paint，鼠标冒泡按**注册顺序倒序**派发：
遮罩（1）比本地菜单（0）后画、在冒泡里**先**收到 mousedown，菜单容器的
`stop_propagation` 来不及拦 —— **菜单内任何左键按下都会先被遮罩吞掉、菜单直接关闭**。

- 真机复现（修复前）：右键文件行 → 点菜单内标题区 → 菜单**直接关闭**（应保持打开）；
  再点「手动选择匹配」→ 毫无反应。即**两个菜单入口此前实际不可用**。

**修法**（`crates/sodam/src/views/local_music.rs`）：

- `local_menu` → `.with_priority(2)`（与队列 / 轨道菜单一致，高于遮罩 1）；
- `match_picker` → `.with_priority(3)`（确认类浮层约定：遮罩 1 < 菜单 2 < 确认 3+ < 登录 10）。

**验证**（2026-10-15 真机截图 `p21-*`）：

- 修复后：点菜单内标题区 → **菜单保持打开**；点「手动选择匹配」→ 弹出
  「选择匹配」浮层（Yang → top-5 候选：Vicky Salamor / JKING8 / Rita Sugiarto /
  Rasti / Andysan，按排名显示、无分数）。
- `fmt --check` / `clippy -D warnings` / `test`（96 个）/ `build --offline -p sodam` 全绿。

## 2026-10-15 — 文档整理：design-plan.md（合并设计稿未完成项与 plan.md）

用户要求把 `sodam-download-design.md` 的**未完成项**与 `plan.md` 的后续计划合并为
`design-plan.md`，并清理两份原文件。执行：

- 新增 `design-plan.md`（仓库根）：下载设计稿未完成项（9 条）+ 后续计划（M2 / M3 /
  B 阶段 / 断点续传）+ 注意事项（含全部已决定项）+ 环境与流程备注 + libresoda
  下载 API 速查附录。
- 删除 `sodam-download-design.md`（未跟踪）与 `plan.md`（已跟踪；删除在工作区，
  随下次提交生效）。
- 全仓库引用改指 `design-plan.md`：本文件（头部 + 两处正文）、`docs/ROADMAP.md`、
  `crates/sodam-core/src/download.rs`、`crates/sodam/src/app.rs`、`app_actions.rs`、
  `views/local_music.rs` 的注释。
- 顺带核对并写入新文档：`start_track` 的本地命中按 `track_id` 对所有播放入口生效
  （旧记录「仅本地音乐 / 已下载两个列表」与代码不符，待真机复验后统一）；`Root::status`
  全 crate 无渲染点（已核实），全局提示目前没有出口。
- 纯文档 / 注释改动，无行为变化；`fmt --all -- --check` 通过（其余编译门未跑）。

## 2026-10-15 — 存量下载目录旁路迁移脚本（旧扁平布局 → image/ + lrc/）

用户实际部署目录 `D:\soft\app\SodaM\down` 是 0.3.x 之前的**扁平布局**（封面 / 歌词
与音频平铺），而当前代码与产品语义已改为 `<下载根>/image/` 与 `<下载根>/lrc/`
（`scan_sidecars` 只读这两个子目录）。补一个一次性维护脚本把存量文件收进新布局。

### 脚本

- **`scripts/migrate-sidecars.py`（新增）**：
  - 判定与 `sodam-core` 的 `scan_sidecars` 对齐：`png/jpg/jpeg/webp → image/`、
    `lrc → lrc/`；按**文件名 stem** 配对（与产品同源，不按清单）。
  - 默认**演练**（不动文件），`--apply` 才执行；`--include-unpaired` 可连无配对音频的
    图片 / 歌词一并迁入（默认留在原地）。
  - 递归子目录；根下的 `image/`、`lrc/` 跳过 —— **幂等**，可重复运行。
  - 冲突：目标已有**内容相同**文件 → 删来源副本（`dedupe`）；**内容不同** →
    来源改名 `"<stem> (2).<ext>"` 移入（`rename`）。用 SHA-256 比对，不做覆盖。
  - 日志（默认 `<root>/migrate-sidecars.log`，UTF-8）逐条记录 move / rename / dedupe
    / 跳过原因，可据此人工回滚；**追加写**，保留历次运行记录。
  - 结尾打印汇总，包括「迁移后仍无封面的音频数 / 无歌词的音频数」，便于对账。
  - 用 `os.walk` + `shutil` 同级的 `os.rename`；不引入第三方依赖。

### 实测（Windows x64，真实部署目录）

- 源目录 `D:\soft\app\SodaM\down`：425 个文件**全部平铺在根** ——
  144 音频（118 mp4 + 26 m4a）、144 图片（143 png + 1 jpg）、137 lrc。
- 演练：281 个旁路文件（144 图 + 137 词）**全部能配对**，根下无孤儿、0 冲突。
- `--apply` 执行：`move 281`、`rename 0`、`dedupe 0`、`failures 0`。
- 执行后状态：根下 145 个文件（144 音频 + 日志）；`image/` = 143 png + 1 jpg（144）；
  `lrc/` = 137 lrc。抽取 `Olly Murs - That Girl`、`Snowhite - Mirror Solitude` 两组
  核对音频 / 图 / 词落位正确。
- 复跑演练：`sidecars 0`、全 `placed` —— 幂等性成立。
- 合成场景测试（临时目录）覆盖真实数据没触发的分支：`dedupe`（同内容删除来源）、
  `rename`（不同内容加序号）、`unpaired`（跳过）、`nested`（递归子目录）——
  8 项断言全过。

### 需要人工注意

- **部署中的 exe 是 10/14 10:43 的旧构建**（早于旁路改子目录的 `2b8e649`，10/15 14:17）。
  旧构建写的是扁平旁路、也读不到新布局 —— 迁移后要让部署目录指向新布局生效，
  需要**重新构建并替换 `D:\soft\app\SodaM\sodam.exe`**（本次未做，工作区未动代码）。
- 迁移**不动音频**；清单 `.sodam-downloads.json` 不在该目录（便携版清单本就不存在，
  该目录是纯落盘目录），未涉及。
- 日志覆盖事故：首次 `--apply` 的逐条日志被随后的验证演练覆盖（脚本原为覆盖写），
  已在日志中补记实际执行结果（281 条、0 失败），脚本已改为**追加写**。

## 2026-10-15 — 合并上游 main 到 feat/0.3.0（v0.2.5–0.2.7 → 0.3.0 线）

用户要求「拉取上游代码的同时保留新功能」。本地 `main` 原停在 `1d812fb`（落后
`origin/main` 9 个提交），已快进到 `f3138d4`（v0.2.7），再整体并入 `feat/0.3.0`。
合并提交 `f015ae3`（本地，未推送）。

### 上游带入的内容（0.2.5 – 0.2.7）

- `perf(cover)`：照 sonora 重做封面缓存 —— 按显示尺寸解码 + 16MB 预算
  （`ui/cover_cache.rs` 大改、`ui/artwork.rs` 重写、`image` crate 打开
  jpeg/webp/gif 特性）。
- `perf(mem)`：整曲解密后 `mem::release_malloc_pressure()` 归还 malloc 空闲页
  （新文件 `sodam-core/src/mem.rs`；`session.rs` 落盘点调用）。
- `fix(ui)`：Windows / macOS 标题栏与面板背景色差（`ui/titlebar.rs`、
  `sidebar` / 内容区下移、根元素统一画渐变、队列面板让开窗口按钮）。
- `feat(playback)`：推荐流 / 电台回传 `played_media`（`play_report.rs`、
  `library.rs`）。
- 依赖升级：`gpui` `68d1d0b → 5ff26c7`、`libresoda` `c0b911b → 928797d`。

### 冲突解决（7 个文件，与只读预演完全一致）

- **版本号族**：`Cargo.toml` / `Cargo.lock`（sodam、sodam-core 两处）/
  `packaging/arch/PKGBUILD` / `README.md`（8 处）—— 保留 **0.3.0** 线。
  依赖段（gpui / libresoda rev）取上游新值。
- **`crates/sodam-core/src/lib.rs`**：`match_metadata`（本线）与 `mem`（上游）并存。
- **`crates/sodam/src/views/lyrics.rs`**：保留本线
  `playback_cover_source` / `download_slot` / `is_downloaded`，吸收上游
  `cover_loading_gate`（切曲 200ms 加载态）与「重试按钮内嵌封面」布局。
- **`crates/sodam/src/ui/player_bar.rs`**：保留本线叠加抽屉（`DRAWER_W`、
  `overlay_panel_width`）、本地曲目封面来源、下载入口；吸收上游
  Windows 标题栏避让（`.pt(STRIP_HEIGHT)`）。

### 验证

- `cargo check --offline --workspace` / `fmt --all -- --check` /
  `clippy --offline --workspace --all-targets -- -D warnings` /
  `test --offline --workspace`（**103** 个）/ `build --offline -p sodam`：全绿。
- i18n：调用 key 293、词典 411、**0 遗漏**。
- 两侧功能抽查：本线（sidebar 本地音乐 / app_download / local_music /
  match_metadata / paths / 叠加抽屉）与上游（mem / cover_cache / titlebar /
  lyrics_cover_gate / played_media）均在。
- **真机冒烟（Windows x64，合并后新构建）**：启动无 panic、stderr 干净；
  截图确认侧边栏含「本地音乐」、播放栏含下载入口、播放页封面与歌词正常
  （上游新封面链路生效）、设置页含下载设置区块。
- 排查记录：`SODAM_NAV=local` 启动时会被启动期异步推荐加载的
  `open_lyrics()` 覆盖（既有 dev 工具竞态，与本次合并无关），截图核对以
  手动点击侧边栏为准。

## 2026-10-16 — 修复：下载失败标签数字错位 + 播放缓存上限（LRU）

用户反馈两件事，先验证再改：下载失败列表无数据却显示数字；缓存希望可限制大小。

### 问题 1：下载失败 tab 的数字挂错分区（`crates/sodam/src/views/downloads.rs`）

**根因**：面板 tab 数量数组的顺序与 `DownloadTab::ALL` 不一致。

```rust
// 旧（bug）：ALL = [Active, Failed, Completed]，这里却是 [active, completed, failed]
let tab_counts = [counts.active(), counts.completed, counts.failed + counts.cancelled];
```

于是「下载失败」标签拿到的是**已完成**数、「已下载」标签拿到的是**失败+取消**数 ——
下载成功后失败标签挂着数字、点进去却是空的。列表过滤本身没错（`is_failed` 已含
Cancelled），错的只是徽章。

**修法**：删掉这个数组，改成按 tab 取值的 `tab_count(counts, tab)`，顺序不再能对错。
新增回归测试 `views::downloads::tests::tab_counts_match_each_partition`。

### 问题 2：新增播放缓存上限（LRU）

- **`crates/sodam-core/src/cache.rs`（新增）**
  - `CacheSettings { limit_mb }`（`0` = 不限，默认）+ `limit_bytes()` / `threshold_bytes()`
    （高水位 = 上限 × 85%）。
  - `enforce_cache_limit(settings, protect)` / `evict_to_threshold(dir, threshold, protect)`：
    只扫**播放缓存目录**（`cache/audio`），把占用淘汰到阈值以下；LRU 依据是文件 mtime。
    淘汰单位是**整首歌**（`<id>-<tag>.m4a` + 其 `.quality` 边车），`.part` 不参与；
    删不掉（Windows 上被占用）就跳过；`protect`（刚写入 / 正在播放）永不删。
  - 5 个单测：不限为 no-op、阈值 = 85%、LRU 删到阈值以下且成对删除、受保护文件不删、
    未超阈值不动。
- **`crates/sodam-core/src/config.rs`**：`Settings` 新增 `cache: CacheSettings`；
  新增往返 / 老配置回退测试，并断言它**不进 `fingerprint()`**（纯本地项）。
- **`crates/sodam-core/src/lib.rs`**：导出 `cache` 模块与 `CacheSettings`。
- **`crates/sodam-core/src/session.rs`**
  - `download_to_cache` 命中缓存时 `touch_cache_file`（刷新 mtime，作为 LRU 最近使用信号）。
  - 落盘成功后调用 `enforce_cache_limit(&self.settings.cache, Some(&path))`（已在后台阻塞
    上下文；显式保护刚写的文件，防「上限小于单曲」的极端配置）。`SODAM_CACHE_LOG=1`
    时打印淘汰结果。
- **`crates/sodam/src/app_actions.rs`**：新增 `apply_cache_settings` —— 保存配置并在后台
  按新上限淘汰一次（不必等下一首下载），完成后刷新设置页占用统计。
- **`crates/sodam/src/views/settings.rs`**：「缓存」区新增「播放缓存上限」`segmented`
  （不限 / 1 / 2 / 5 / 10 / 20 GB）；任意值改 `config.json` 的 `cache.limit_mb`。
- **`crates/sodam/src/ui/i18n.rs`**：新增 4 条英文（播放缓存上限 / 说明 / 不限 / 已保存）。

### 顺带修：`clear_cache` 漏删封面原图（`crates/sodam-core/src/audio.rs`）

`clear_cache()` 原先只删各缓存目录的**顶层文件**，而封面原图在 `covers/original/`
子目录里 —— 清不掉；但 `cache_stats()` 是**递归**统计，于是「清空缓存」后封面占用
数字不变（清 / 统计口径不一致）。改为 `remove_files_recursive`（递归；不跟进符号链接）。

### 影响面与已知取舍

- 缓存上限**只管播放缓存**：封面缓存与下载目录（`down/`）都不受影响；内存里的解码
  封面缓存本就有上界（`cover_cache.rs`，16MB / 256 条）。
- LRU 用 mtime：命中刷新失败（如只读句柄）时退化为 FIFO，不影响正确性。
- 上限默认 `0`（不限），行为与改动前一致；只有用户显式设定后才淘汰。
- 新增/改动与下载设置同构：预设在 UI、任意值走 `config.json`（与下载目录 / 命名模板
  的既有约定一致）。

### 验证（Windows x64，`CARGO_BUILD_JOBS=6`）

- `cargo check --offline -p sodam-core` / `-p sodam`：通过。
- `cargo fmt --all -- --check`：通过。
- `cargo clippy --offline -p sodam-core -p sodam --all-targets -- -D warnings`：通过
  （先修掉 `derivable_impls` 与 `sort_by_key` 两条）。
- `cargo test --offline -p sodam-core`：**88** 通过（原 81 + cache 5 + config 1 + 既有）；
  `-p sodam`：**22** 通过（含新增 tab 计数回归）。
- **UI 未做真机验证**（本机无桌面截图手段）：设置页新增的「播放缓存上限」分段控件
  需要人工过一眼。
### 版本与提交

- 版本 `0.3.0` → `0.3.1`：`Cargo.toml`（workspace 版本）与 `packaging/arch/PKGBUILD`（`pkgver`）；
  `Cargo.lock` 由构建自动刷新。README 里的 `v0.3.0` 下载示例指向**已发布的 Release 产物**，
  在真正发版前保持不动。
- `cargo build --offline -p sodam` 重建 `target/debug/sodam.exe`：`cargo check` / `cargo test`
  不更新二进制，`scripts/run.sh` 跑的是旧 exe（本次踩过）。
- 本次改动提交并推送至 `origin/feat/0.3.0`。



## 2026-10-23 — 把 feat/0.3.0 的下载 / 本地音乐 / 便携模式并入 v0.3.5

0.3.5 从 `main`（0.3.4）拉出，再合并 `feat/0.3.0`（0.3.1）。两条线从 `f3138d4`（0.2.7）分叉，
各自演化：main 做了搜索框重写、单实例、消息提示、贡献者墙、网站与隐私文档；feat 做了下载、
本地音乐、便携模式。合并时 12 个文件冲突、28 处冲突块。

### 取舍：缓存上限只保留 main 的实现

两边都实现了「播放缓存上限」，但接口不同，**不能并存**：

- main：`Settings::audio_cache_limit_mb`（默认 10 GiB，`cache::trim_audio` + `CacheLease` 租约）
- feat：`Settings::cache: CacheSettings`（默认 0 = 不限，`enforce_cache_limit` + 85% 高水位）

合并后**保留 main 这一套**：它多了租约保护（正在播放 / 待装载的文件不会被删）、写者串行化、
`.part` 新鲜度判断，语义也更完整。feat 的 85% 高水位没有保留 —— main 是「超限才删」。
因此删掉了 `CacheSettings`、`EvictedReport`、`settings.cache` 字段与对应测试；
`session.rs` 落盘后的淘汰改为 `cache::trim_audio(self.settings.audio_cache_limit_mb)`；
`app_actions::apply_cache_settings` 同样改调 `trim_audio`。

### 取舍：缓存根目录跟随便携模式

main 的 `cache::root_dir` 直接读 `dirs::cache_dir()`，不认识便携模式；feat 的 `paths::cache_dir`
才认。合并后 `root_dir` 先看 `SODAM_CACHE_DIR` 覆盖，其余交给 `paths::cache_dir()` ——
否则便携版会把缓存写进用户目录，与「插 U 盘就能带走」冲突。

### 播放引擎：本地文件也要取租约

main 的 `PlaybackEngine::load` 已改成收 `CachedTrack`（含租约）。feat 新增的本地文件播放
（下载目录里的文件，不进播放缓存）仍在传 `path + quality`。新增
`CachedTrack::from_local(path, quality)`：仍取一份租约，避免「清空缓存」与正在播放的文件撞车；
两处调用点改为走它。

### 其余冲突

- `Cargo.toml` / `Cargo.lock` / `packaging/arch/PKGBUILD`：版本取 main 的 `0.3.4`（本分支后续发 0.3.5）。
- `Cargo.toml` 的 `libresoda` rev 取 main 的 `44df096`（feat 是旧的 `928797d`）。
- `lib.rs`：两边模块与导出都保留（`contributors` + `download` / `download_library`）。
- `config.rs`：保留 main 的 `config_path`（走 `paths`）；两边的测试都保留。
- `audio.rs`：`cache_dir` / `cover_cache_dir` / `clear_cache` 取 main 版（租约 + 递归清理），
  同时保留 feat 新增的 `probe_duration` / `probe_mp4_duration`。
- `session.rs`：`cover_path` 保留 feat 的本地封面分支；落盘流程用 main 的 rename + 校验写法。
- `app.rs`：队列抽屉改为「宽度按视口算 + `own_titlebar` 两个参数都传」，同时挂上下载面板。
- `app_actions.rs`：贡献者墙与 `is_local_track` 并存；心跳里同时保留 `maintain_cache` 与 `sync_downloads`。
- `player_bar.rs`：`queue_drawer` 收 `width` 与 `own_titlebar`；`queue_drawer_inner` 用
  `Option<f32>` 区分抽屉 / 内嵌；标题栏让位改回 `own_titlebar`（main 的跨平台语义，feat 只判 Windows）。
- `README.md`：版本号一律取 0.3.4；保留 feat 的 Windows 便携版段落。

### 验证（Windows x64，`CARGO_BUILD_JOBS=6`）

- `cargo check --workspace`：通过。
- `cargo clippy --workspace --all-targets -- -D warnings`：通过。
- `cargo test --workspace`：`sodam` 73 通过、`sodam-core` 103 通过，0 失败。
- 依赖：main 把 `libresoda` 升到 `44df096`，本地无缓存，第一次 `cargo check` 需联网（`--offline` 会失败）。
- **UI 未做真机验证**：需 `scripts/run.sh` 或直接跑 `target/debug/sodam.exe` 过一遍下载面板、
  本地音乐页、队列抽屉与设置页。

## 2026-10-23 — 修 v0.3.5 合并后的两处 UI 问题

合并 `feat/0.3.0` 后真机发现两处，均属合并引入或合并遗漏，与 main / feat 各自的功能无关。

### 1. 设置页出现两套「播放缓存上限」

合并时 main 的缓存区（`−1 GiB / 5 / 10 / 20 / 50 / +1 GiB`，单位 GiB）与 feat 残留的
`setting_row("播放缓存上限", segmented(不限/1/2/5/10/20 GB))` 同时留在页面上，
两者写同一个 `audio_cache_limit_mb`，但单位不同，看起来像「改了没反应」。

- 删除 feat 残留的 `limit_control` 及其 `setting_row`，只保留 main 那套（`set_cache_limit_mb`）。
- 同时删除合并时新增、但已无调用点的 `Root::apply_cache_settings`
  —— main 的 `set_cache_limit_mb` 已经包含保存 + 后台淘汰 + 刷新统计。

### 2. 下载面板的关闭按钮与窗口按钮重叠

队列抽屉在自绘标题栏下会 `pt(STRIP_HEIGHT)` 让出窗口按钮所在的一条
（`player_bar::queue_drawer_inner`）；下载面板没有，工具栏的 `x` 顶到了窗口的 `x`。

- `views/downloads.rs`：`panel` 新增 `own_titlebar: bool` 参数，
  为真时整体 `pt(STRIP_HEIGHT)`。
- `app.rs`：调用处传入 `own_titlebar`。

### 验证

- `cargo check -p sodam` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check`：通过。
- `cargo test --workspace`：`sodam` 73 通过、`sodam-core` 103 通过。
- **UI 未做真机验证**：需启动 `target/debug/sodam.exe` 确认设置页只剩一套上限控件、
  下载面板关闭按钮不再与窗口按钮重叠。

### 未处理（已记录，交上游）

seek 之后 `PlayReporter::played_ms` 不重置 —— 属 `main` 的既有口径问题
（`play_report.rs:161-163` 按墙钟累加，从不读 `position_seconds`）。
详见 [docs/UPSTREAM-seek-played-ms.md](docs/UPSTREAM-seek-played-ms.md)，本次未改代码。

## 2026-10-23 — 版本号 0.3.4 → 0.3.5

- `Cargo.toml`（workspace `version`）、`packaging/arch/PKGBUILD`（`pkgver`）、`Cargo.lock` 里
  `sodam` / `sodam-core` 两个本地包的 `version` 改为 `0.3.5`。
- `Cargo.lock` 里还有 4 处 `0.3.4`（`arg_enum_proc_macro` / `getrandom` / `glob` /
  `windows-result`），那是第三方依赖的版本，**未改动**。
- README 里的 `v0.3.4` 下载示例指向已发布的 Release 产物，在真正发版前保持不动。
- 验证：`cargo check --workspace` / `clippy --workspace --all-targets -D warnings` /
  `fmt --all -- --check` 通过；`cargo test --workspace` 73 + 103 通过。
  `target/debug/sodam.exe` 已重建为 v0.3.5。
