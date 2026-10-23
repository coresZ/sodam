# 架构

## 总览

```text
┌────────────────────────── crates/sodam（GPUI 层） ───────────────────────────┐
│ Root                                                                          │
│  ├── 导航 / 设置 / 搜索输入 / 播放队列缓存 / 封面缓存 / ambient colors        │
│  ├── app_actions.rs    播放、队列、收藏、歌单、推荐、设置、封面动作             │
│  ├── app_search.rs     搜索、艺人页、专辑页动作                                │
│  ├── app_download.rs   下载入队、面板控制、打开文件夹                          │
│  ├── ui::sidebar       侧边栏、品牌、导航                                     │
│  ├── views::*          Discover / Scenes / Search / Artist / Album / ...      │
│  ├── views::downloads  下载管理面板（进行中 / 已完成 / 失败）                  │
│  ├── ui::player_bar    播放控制、进度、音量、播放队列                          │
│  ├── ui::theme         Dark/Light、ambient color、尺寸、共享颜色               │
│  └── tray.rs           StatusNotifierItem 托盘与托盘菜单                      │
└───────────────────────────────────────────────────────────────────────────────┘
                                 │
                                 ▼
┌──────────────────── crates/sodam-core（领域层，无 GPUI） ─────────────────────┐
│ Settings       配置文件 + 环境变量补全（含 download 块）                       │
│ Session        Cookie、AppCredentials、签名 provider、音质偏好                │
│ library        推荐 / 听歌模式 / 搜索 / 歌单 / 收藏 / 艺人 / 专辑 / 歌词       │
│ download       DownloadManager 队列、并发调度、落盘、封面 / 歌词旁路           │
│ Queue          当前曲目、历史、下一首、随机 / 顺序 / 单曲循环                  │
│ PlaybackEngine rodio 线程；接收 Load / Play / Pause / Seek / Volume 命令       │
│ AudioColor     封面主色提取                                                   │
└───────────────────────────────────────────────────────────────────────────────┘
                                 │
                                 ▼
                       libresoda（汽水接口、取流、解密、签名）
                                 │
                       libmssdk（可选远程签名服务）
```

## 状态模型

`Root` 是 GPUI 侧唯一根实体。子模块不保存第二份全局状态，只读 `&Root` 或通过
`cx.listener` / entity update 派发动作。列表滚动句柄、菜单位置、搜索输入等 UI 状态
放在 `Root`；缓存数据用 `Arc` 共享，避免渲染路径深拷贝。

当前曲目的取值顺序是：

```text
pending_track（正在装载的新歌）
  → queue.current()（已生效的当前歌）
```

这样可以避免切歌时出现“界面显示 A、耳朵听到 B”的竞态。

## 线程与异步

libresoda 的 HTTP 和下载是阻塞的；GPUI 渲染不能阻塞。规则如下：

1. 网络请求、文件下载、解密、封面缩放放到 `cx.background_spawn` 或专用线程。
2. 完成后用 `cx.spawn` + entity update 回到 GPUI 实体上下文，然后 `cx.notify()`。
3. 带分页 / 可取消语义的请求使用 `request_seq` 丢弃过期结果。
4. 封面通过 `CoverPool` 有界并发请求，渲染路径只查本地缓存。
5. 音频线程独立于 GPUI，通过 channel 接收命令，通过 snapshot 共享状态。

## 播放链路

```text
TrackItem + cache path
  → CachedTrack（本地路径、音质、缓存租约）
  → PlaybackEngine::load(track, cached) → 装载序号
  → rodio Decoder + Sink
  → snapshot: track_id / title / playing / position / duration / quality / error
```

播放结束时音频线程标记 `finished`；GPUI 心跳读取 snapshot，调用 `next_track`。
签名不足或设备不匹配时，libresoda 可能只返回试听；错误会进入状态行或播放页重试区。

音频装载成功/失败都回执 `load_seq`，心跳据此退出 loading，不再只比较 track id。
输出设备错误或非暂停状态下 15 秒无进度会释放失效输出并报告错误；用户再次点播放时
重新装载缓存、打开设备，不会因为启动/休眠后的一次设备失败而永远不可播放。

系统默认输出设备被切换时（音响 → USB 耳机），播放线程每 2 秒探测一次默认设备 ID，
变化后换新输出并从原位置续播 —— Windows(WASAPI) / macOS(CoreAudio) 的流在创建时就
绑死设备，不重建就得重启应用才生效。Linux 的 cpal ALSA 后端恒为单一的 `default` 设备，
现有流由 PipeWire/WirePlumber 自动迁到新的默认节点，因此探测不会触发（行为与修复前一致）。

## 播放偏好与磁盘缓存

- `PreferencesWriter` 是后台单写者：音量/静音/缓存上限防抖保存，只合并这些字段，
  避免覆盖刚修改的账号/主题。所有配置写入用同一进程内锁和同目录原子替换，退出刷尾。
- `cache` 默认限制音频为 10 GiB，设置页可调整；后台启动/定时维护，不在渲染线程扫盘。
  实际缓存命中更新音频 mtime，巡检的 `is_cached` 不刷新最近使用时间。
- `CacheLease` 从下载/缓存命中，经后台返回值、音频命令一直传到播放器寿命结束，
  淘汰与手动清理都跳过被租用的音频及其 sidecar。同一缓存目标的下载写者串行化。
- 清理不跟随符号链接，覆盖封面原图子目录；近期 `.part` 保留，超过 24 小时可清理。
  上限是软上限：受保护文件允许暂时超额，释放后由下一轮维护淘汰。

## 导航与详情页

`Nav` 包含固定入口和详情页：`Discover`、`Listening Modes`、`Search`、`Liked Music`、
`My Playlists`、`Artist`、`Album`、`Settings`、`Now Playing`。

- 详情页使用 `nav_history` 做返回。
- 歌手页 / 专辑页由 `app_search.rs` 拉取并缓存在 `open_artist` / `open_album`。
- 曲目里的歌名和歌手可以打开专辑 / 音乐人页；点击事件会 stop propagation，
  避免触发行点击播放。

## 队列与历史

`Queue` 保存 upcoming 列表和当前索引；播放后的曲目进入 `played_history`。
队列展示分为已播放、正在播放、接下来三段。随机播放会重建 upcoming 顺序；
单曲循环在自然结束时重复当前曲。

## 下载

`download` 模块把「队列 + 并发 + 落盘」都放在领域层，UI 只读快照：

```text
Root::downloads (DownloadManager)
  ├── 调度线程            按 concurrent 把 Queued 派给工作线程（只跑状态机，无 IO）
  ├── 工作线程 × N        get_download_info → 拉流 → 解密 → 校验 → rename
  └── Shared             任务表 / 事件队列 / 设置快照（Mutex + 原子版本号）
```

- **快照渲染**：`Root::download_cache` 存 `(revision, Arc<Vec<DownloadTask>>)`，
  心跳里比对版本号才重新克隆，面板每帧不做深拷贝。
- **并发额度是数出来的**：`State::running()` 现场统计，不用计数器 ——
  暂停 / 取消会在传输途中改写状态，计数器漏减一次队列就永久卡死（已加回归测试）。
- **下载音质与播放音质分离**：任务创建时快照档位，之后改设置不影响已在队列的任务。
- **中止语义**：暂停 / 取消置位 `AtomicBool`，工作线程下一次读就退出并删掉 `.part`；
  自动重试的退避期间也会检查这个标志，保证「取消」立刻生效。
- **失败分类**：`DownloadFailure { retryable }` 区分「网络抖动（可重试）」与
  「试听 / 无权限 / 磁盘满（重试无意义）」，UI 直接照着展示。
- **旁路元数据**：封面与 `.lrc` 是加分项，失败只记日志、不影响下载本身。

## 主题、ambient color 与 i18n

- 主题支持 `Dark`、`Light` 和首次启动跟随系统。
- 封面主色提取后写入 `ambient_colors`，再由 `theme::set_ambient_rgb` 影响
  背景、表面、边框、强调色。
- 左上角与设置页的四色 logo 基于当前封面主色生成：主色、互补色、两个相近色。
- 界面文案使用 `ui/i18n.rs`；中文原文是稳定 key，英文由词典映射。

## 打包与运行时

- Linux ARM64 使用 `packaging/arch/PKGBUILD` 生成 pacman 包。
- 安装内容：`/usr/bin/sodam`、desktop entry、hicolor icon。
- GUI 通过 `scripts/run.sh` 启动时带 `MemoryMax=2G` 护栏。
- 托盘和窗口关闭逻辑使用显式退出模式：关闭主窗口保留进程，退出走托盘菜单。
