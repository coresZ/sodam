//! 打印运行期目录解析结果，用来验证便携模式。
//!
//! 用法：
//!
//! ```text
//! cargo run -p sodam-core --example paths_probe          # 安装版布局
//! SODAM_PORTABLE=1 cargo run -p sodam-core --example paths_probe
//! ```
//!
//! 更贴近真实的验证：把编译好的可执行文件复制到一个带 `portable.txt`
//! 的目录里再运行 —— 这样走的是「exe 同级标记文件」判定，而不是环境变量。

fn main() {
    println!("portable      : {}", sodam_core::paths::is_portable());
    println!("portable root : {:?}", sodam_core::paths::portable_root());
    println!(
        "config dir    : {}",
        sodam_core::paths::config_dir().display()
    );
    println!(
        "config file   : {}",
        sodam_core::paths::config_dir()
            .join("config.json")
            .display()
    );
    println!(
        "cache dir     : {}",
        sodam_core::paths::cache_dir().display()
    );
    println!(
        "audio cache   : {}",
        sodam_core::audio::cache_dir().display()
    );
    println!(
        "cover cache   : {}",
        sodam_core::audio::cover_cache_dir().display()
    );
    println!(
        "download dir  : {}",
        sodam_core::download::default_download_dir().display()
    );
}
