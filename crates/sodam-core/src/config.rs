//! 应用配置（`~/.config/sodam/config.json`）。

use crate::cache::CacheSettings;
use crate::download::DownloadSettings;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// 与 libresoda / libmssdk 对接需要的全部设置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 签名服务地址（libmssdk 的 `/sign`，VIP 整曲必需）。
    pub signer_url: String,
    /// 签名服务 token（`Authorization: Bearer`），服务端没开鉴权时留空。
    pub signer_token: String,
    /// 登录 Cookie（扫码登录成功后写入）。
    pub cookie: String,
    /// 设备指纹：必须与签名器一致（签名器 `/config` 可查）。
    pub device_id: String,
    pub iid: String,
    pub fp: String,
    /// 音质偏好：`best` / `lossless` / `highest` / `medium` / `low`。
    pub quality: String,
    /// 界面主题：`dark` / `light`；空 = 第一次启动跟随系统偏好。
    pub theme: String,
    /// 界面语言：`zh` / `en`；空或 `auto` = 跟随系统语言。
    pub language: String,
    /// 播放/暂停快捷键（gpui 的 keystroke 写法，如 `space`、`ctrl-left`）。
    /// 空串 = 用内置默认值 `DEFAULT_SHORTCUT_*`。
    pub shortcut_toggle: String,
    /// 上一首。
    pub shortcut_previous: String,
    /// 下一首。
    pub shortcut_next: String,
    /// 下载设置（音质 / 目录 / 命名 / 并发…）；老配置缺这一段时用默认值。
    pub download: DownloadSettings,
    /// 播放缓存设置（上限…）；老配置缺这一段时用默认值。
    pub cache: CacheSettings,
}

/// 播放控制快捷键的内置默认值。
///
/// 播放/暂停三平台都用 `Space`；上一首/下一首按平台分开：
/// * macOS 用 `⌘←` / `⌘→`（Apple Music 就是这两个键）。**不能用 `⌃←`/`⌃→`**：
///   那是系统「切换桌面（Move left/right a space）」的默认快捷键，真键盘按下去
///   会被系统先吃掉，应用根本收不到（`com.apple.symbolichotkeys` 里的 key 79/81）。
/// * Windows / Linux 用 `Ctrl+←` / `Ctrl+→`（浏览器与播放器的通行做法，系统没占用）。
///
/// 播放操作绑在 `!input` 上下文上，所以在文本框里 `⌘←` 仍是「行首」，不受影响。
pub const DEFAULT_SHORTCUT_TOGGLE: &str = "space";
#[cfg(target_os = "macos")]
pub const DEFAULT_SHORTCUT_PREVIOUS: &str = "cmd-left";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_SHORTCUT_PREVIOUS: &str = "ctrl-left";
#[cfg(target_os = "macos")]
pub const DEFAULT_SHORTCUT_NEXT: &str = "cmd-right";
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_SHORTCUT_NEXT: &str = "ctrl-right";

/// 配置为空（或只有空白）时回退到内置默认值。
fn effective_shortcut<'a>(value: &'a str, default: &'a str) -> &'a str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default
    } else {
        trimmed
    }
}

/// 项目自建的签名服务（sodahub-org 部署）：开箱即用，可在设置里改成别的实例。
pub const DEFAULT_SIGNER_URL: &str = "http://103.236.75.83:8899/sign";
/// 上面那台服务的 token（等同取流能力，别外传；换服务时在设置里替换）。
pub const DEFAULT_SIGNER_TOKEN: &str = "b356ad402bc31ee9e5a99cfdbf41eaa39961a562251c3359";

/// 换服务前的那台内置实例，2026-10 起已下线。
/// 老版本的默认值会落进 config.json，所以读取时要把它们迁到当前默认值，
/// 否则老用户会一直指着一台死掉的服务。
const LEGACY_SIGNER_URL: &str = "http://222.186.10.201:8921/sign";
const LEGACY_SIGNER_TOKEN: &str = "05f8089b8c5f60c63f2a6dcfe1028d28ee2725a504f3e59b";

impl Default for Settings {
    fn default() -> Self {
        Self {
            signer_url: DEFAULT_SIGNER_URL.to_string(),
            signer_token: DEFAULT_SIGNER_TOKEN.to_string(),
            cookie: String::new(),
            device_id: String::new(),
            iid: String::new(),
            fp: String::new(),
            // 空 = 自动：登录后按账号 VIP 情况挑能用的最高档（见 Session::refresh_quality）
            quality: String::new(),
            theme: String::new(),
            language: String::new(),
            shortcut_toggle: String::new(),
            shortcut_previous: String::new(),
            shortcut_next: String::new(),
            download: DownloadSettings::default(),
            cache: CacheSettings::default(),
        }
    }
}

impl Settings {
    pub fn config_path() -> PathBuf {
        crate::paths::config_dir().join("config.json")
    }

    /// 读配置；文件不存在时返回默认值（不报错）。
    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    pub fn load_from(path: &Path) -> Self {
        let Some(settings) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
        else {
            return Self::default();
        };
        let (settings, migrated) = settings.migrated();
        // 迁移后立刻落盘：用户下次打开配置文件看到的、以及设置页显示的，都是真正生效的那份。
        // 写失败不影响启动，内存里的值已经是迁移后的。
        if migrated {
            let _ = settings.save_to(path);
        }
        settings
    }

    /// 把老版本的默认端点迁到当前默认值。
    ///
    /// 只认「等于旧默认值」这一种情况，用户填过别的地址就原样保留；
    /// token 同理，避免覆盖用户自己给别的实例配的 token。
    /// 返回是否发生了改动。
    fn migrated(mut self) -> (Self, bool) {
        if self.signer_url.trim() != LEGACY_SIGNER_URL {
            return (self, false);
        }
        self.signer_url = DEFAULT_SIGNER_URL.to_string();
        if self.signer_token.trim() == LEGACY_SIGNER_TOKEN {
            self.signer_token = DEFAULT_SIGNER_TOKEN.to_string();
        }
        (self, true)
    }

    /// 写配置（自动建目录）。
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::config_path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// 是否已经具备"能放 VIP 整曲"的条件（cookie + 签名服务）。
    pub fn is_ready_for_vip(&self) -> bool {
        !self.cookie.trim().is_empty() && !self.signer_url.trim().is_empty()
    }

    /// 影响网络请求的字段指纹：变了就必须重建会话（下载管理器据此决定是否更新设置）。
    ///
    /// 故意**不含**主题 / 语言 / 快捷键 / 下载偏好这些纯界面项，
    /// 否则改一次设置就要重建一遍会话。
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.cookie.hash(&mut hasher);
        self.signer_url.hash(&mut hasher);
        self.signer_token.hash(&mut hasher);
        self.device_id.hash(&mut hasher);
        self.iid.hash(&mut hasher);
        self.fp.hash(&mut hasher);
        self.quality.hash(&mut hasher);
        hasher.finish()
    }

    /// 生效的播放/暂停快捷键（配置为空时用内置默认值）。
    pub fn shortcut_toggle(&self) -> &str {
        effective_shortcut(&self.shortcut_toggle, DEFAULT_SHORTCUT_TOGGLE)
    }

    /// 生效的上一首快捷键。
    pub fn shortcut_previous(&self) -> &str {
        effective_shortcut(&self.shortcut_previous, DEFAULT_SHORTCUT_PREVIOUS)
    }

    /// 生效的下一首快捷键。
    pub fn shortcut_next(&self) -> &str {
        effective_shortcut(&self.shortcut_next, DEFAULT_SHORTCUT_NEXT)
    }

    /// 用环境变量补全空字段（`SODA_COOKIE` / `QISHUI_SIGNER_URL` / `QISHUI_SIGNER_TOKEN`…），
    /// 便于先用 shell 跑起来，再逐步落到配置文件。
    pub fn merged_with_env(mut self) -> Self {
        let pairs = [
            ("SODA_COOKIE", &mut self.cookie),
            ("QISHUI_SIGNER_URL", &mut self.signer_url),
            ("QISHUI_SIGNER_TOKEN", &mut self.signer_token),
            ("SODA_DEVICE_ID", &mut self.device_id),
            ("SODA_IID", &mut self.iid),
            ("SODA_FP", &mut self.fp),
            ("SODAM_QUALITY", &mut self.quality),
        ];
        for (key, slot) in pairs {
            if slot.trim().is_empty() {
                if let Ok(value) = std::env::var(key) {
                    if !value.trim().is_empty() {
                        *slot = value.trim().to_string();
                    }
                }
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("sodam-cfg-{}", std::process::id()));
        let path = dir.join("config.json");
        let settings = Settings {
            signer_url: "http://127.0.0.1:8899/sign".into(),
            cookie: "sessionid_ss=x".into(),
            ..Default::default()
        };
        settings.save_to(&path).expect("save");

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.signer_url, settings.signer_url);
        assert_eq!(loaded.cookie, settings.cookie);
        assert!(loaded.theme.is_empty());
        assert!(loaded.language.is_empty());
        assert!(loaded.is_ready_for_vip());
        assert!(!Settings::default().is_ready_for_vip());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老版本把旧默认地址写进了 config.json，读取时要自动换成当前默认值并落盘。
    #[test]
    fn migrates_legacy_default_signer() {
        let dir = std::env::temp_dir().join(format!("sodam-cfg-legacy-{}", std::process::id()));
        let path = dir.join("config.json");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            &path,
            format!(
                r#"{{"signer_url":"{LEGACY_SIGNER_URL}","signer_token":"{LEGACY_SIGNER_TOKEN}","cookie":"sessionid_ss=x","quality":"lossless"}}"#
            ),
        )
        .expect("write");

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.signer_url, DEFAULT_SIGNER_URL);
        assert_eq!(loaded.signer_token, DEFAULT_SIGNER_TOKEN);
        // 迁移不碰其它字段
        assert_eq!(loaded.cookie, "sessionid_ss=x");
        assert_eq!(loaded.quality, "lossless");

        // 迁移结果要落盘，否则用户看到的配置文件仍是死地址
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains(DEFAULT_SIGNER_URL));
        assert!(!text.contains("222.186.10.201"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用户自己填的实例原样保留；旧地址配自定义 token 时只换地址，不动 token。
    #[test]
    fn keeps_user_supplied_signer() {
        let dir = std::env::temp_dir().join(format!("sodam-cfg-custom-{}", std::process::id()));
        let path = dir.join("config.json");

        let custom = Settings {
            signer_url: "http://signer.example.com:8899/sign".into(),
            signer_token: "my-own-token".into(),
            ..Default::default()
        };
        custom.save_to(&path).expect("save");
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.signer_url, custom.signer_url);
        assert_eq!(loaded.signer_token, custom.signer_token);

        // 旧地址 + 自定义 token：地址迁移，token 保留
        let mixed = Settings {
            signer_url: LEGACY_SIGNER_URL.into(),
            signer_token: "my-own-token".into(),
            ..Default::default()
        };
        mixed.save_to(&path).expect("save");
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.signer_url, DEFAULT_SIGNER_URL);
        assert_eq!(loaded.signer_token, "my-own-token");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 快捷键：默认值、空串回退、老配置缺字段也能读。
    #[test]
    fn shortcut_defaults_and_fallback() {
        let settings = Settings::default();
        assert_eq!(settings.shortcut_toggle(), DEFAULT_SHORTCUT_TOGGLE);
        assert_eq!(settings.shortcut_previous(), DEFAULT_SHORTCUT_PREVIOUS);
        assert_eq!(settings.shortcut_next(), DEFAULT_SHORTCUT_NEXT);

        let custom = Settings {
            shortcut_toggle: "alt-k".into(),
            // 只有空白也算未设置
            shortcut_previous: "   ".into(),
            ..Default::default()
        };
        assert_eq!(custom.shortcut_toggle(), "alt-k");
        assert_eq!(custom.shortcut_previous(), DEFAULT_SHORTCUT_PREVIOUS);

        // 老配置文件里没有这三个字段
        let parsed: Settings = serde_json::from_str(r#"{"cookie":"x"}"#).expect("老配置应能解析");
        assert_eq!(parsed.shortcut_next(), DEFAULT_SHORTCUT_NEXT);
        assert!(parsed.shortcut_toggle.is_empty());
    }

    /// 下载设置：老配置缺 `download` 块时用默认值，且能随配置一起往返。
    #[test]
    fn download_settings_round_trip_and_legacy_defaults() {
        // 老配置文件里完全没有 download 字段
        let parsed: Settings =
            serde_json::from_str(r#"{"cookie":"x","quality":"lossless"}"#).expect("老配置应能解析");
        assert_eq!(parsed.download, DownloadSettings::default());
        assert_eq!(parsed.download.naming, crate::download::DEFAULT_NAMING);

        let dir = std::env::temp_dir().join(format!("sodam-cfg-dl-{}", std::process::id()));
        let path = dir.join("config.json");
        let settings = Settings {
            download: DownloadSettings {
                quality: "highest".into(),
                dir: "/tmp/sodam".into(),
                naming: "{title}".into(),
                write_cover: false,
                write_lyrics: false,
                concurrent: 1,
                retry_count: 0,
                overwrite: true,
            },
            ..Default::default()
        };
        settings.save_to(&path).expect("save");
        assert_eq!(Settings::load_from(&path).download, settings.download);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存上限：老配置缺 `cache` 块时用默认值（不限），且能随配置一起往返；
    /// 它是纯本地项，不能影响会话指纹。
    #[test]
    fn cache_settings_round_trip_and_legacy_defaults() {
        let parsed: Settings = serde_json::from_str(r#"{"cookie":"x"}"#).expect("老配置应能解析");
        assert_eq!(parsed.cache, CacheSettings::default());
        assert_eq!(parsed.cache.limit_mb, 0);

        let dir = std::env::temp_dir().join(format!("sodam-cfg-cache-{}", std::process::id()));
        let path = dir.join("config.json");
        let settings = Settings {
            cache: CacheSettings { limit_mb: 2048 },
            ..Default::default()
        };
        settings.save_to(&path).expect("save");
        assert_eq!(Settings::load_from(&path).cache.limit_mb, 2048);
        assert_eq!(
            Settings::default().fingerprint(),
            settings.fingerprint(),
            "缓存上限是本地项，不该进入会话指纹"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 指纹只跟随网络相关字段：改主题 / 语言 / 下载偏好不该重建会话。
    #[test]
    fn fingerprint_tracks_network_fields_only() {
        let base = Settings::default();
        let fingerprint = base.fingerprint();

        let mut themed = base.clone();
        themed.theme = "light".into();
        themed.language = "en".into();
        themed.shortcut_next = "alt-j".into();
        themed.download.dir = "/tmp/x".into();
        assert_eq!(themed.fingerprint(), fingerprint);

        let mut relogin = base.clone();
        relogin.cookie = "sessionid_ss=x".into();
        assert_ne!(relogin.fingerprint(), fingerprint);

        let mut requality = base.clone();
        requality.quality = "highest".into();
        assert_ne!(requality.fingerprint(), fingerprint);
    }
}
