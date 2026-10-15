//! 内存回收辅助。
//!
//! macOS 的 malloc 在释放大块内存（几百 KB 以上，例如无损音频解密时的
//! 「加密整曲 + 明文 + mdat」多份缓冲）后，默认只做 `MADV_FREE`：页被标记
//! 为「可回收」，但不会立刻还给系统。于是 `top` 的 MEM / RSS 会长期虚高
//! （实测无损整曲下载解密后残留 ~280MB），而 Activity Monitor 的
//! phys_footprint 并不包含这部分。
//!
//! `malloc_zone_pressure_relief(NULL, 0)` 会把这些已释放的页真正归还系统，
//! 需要时再按需缺页分配，不影响正确性。

/// 把 malloc 已释放但仍保留的页归还系统。
///
/// 在「下载 + 解密整曲」这类会产生大块临时缓冲的阻塞任务结束后调用。
#[cfg(target_os = "macos")]
pub fn release_malloc_pressure() {
    extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // SAFETY: 该函数由 libSystem 提供，传 NULL zone 表示释放所有 zone 的空闲页。
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

/// 非 macOS 平台没有对应的 malloc 行为，是空操作。
#[cfg(not(target_os = "macos"))]
pub fn release_malloc_pressure() {}

#[cfg(test)]
mod tests {
    #[test]
    fn release_is_callable_and_safe() {
        // 只是确认 FFI 符号可解析、调用不会崩（有空闲页时才会真正归还）。
        let big: Vec<Vec<u8>> = (0..4).map(|_| vec![1u8; 8 * 1024 * 1024]).collect();
        drop(big);
        super::release_malloc_pressure();
    }
}
