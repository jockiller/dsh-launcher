//! macOS Dock 图标「运行中会话」动态指示。
//!
//! 有会话正在执行回合时，图标右下角的电源按钮圆盘做缓慢的红绿渐变呼吸；
//! 无运行会话时恢复原始图标。实现：解码打包图标 → 降采样到 Dock 实际显示
//! 所需的中等分辨率 → 识别电源按钮的绿色像素盘 → 每帧按 HSL 色相轴即时重
//! 着色并编码 PNG → 经 `run_on_main_thread` 调用
//! `NSApplication.setApplicationIconImage`。
//!
//! 内存策略：不预生成整段帧序列，只常驻一份降采样后的 base RGBA（约 260KB）
//! 与按钮像素索引；每帧在后台线程即时重着色 + PNG 编码（远低于帧间隔），
//! 常驻内存不随帧数增长。
//!
//! 定位与 session_monitor 相同：纯外挂观察者，独立线程，所有失败最多产出一条
//! 日志后静默退出；非 macOS 平台为 no-op，任何情况下不影响现有功能。

/// 闪烁帧的停留时长。红绿两态各停留 300ms ≈ 0.6s 一个切换周期，
/// 即“工作状态闪烁”节奏。
#[cfg(target_os = "macos")]
const FRAME_PERIOD: std::time::Duration = std::time::Duration::from_millis(300);
/// Dock 图标工作分辨率上限（短边）。Dock 实际显示 ≤ 128pt，256px 足够清晰，
/// 相比原始 512px 把像素处理量降低 4 倍。
#[allow(dead_code)]
const WORK_SIZE_LIMIT: usize = 256;
/// 图标内容占画布的比例。Apple 图标栅格要求方形容器约 824/1024 ≈ 0.80，
/// 系统渲染的图标都遵循这一留白；直接铺满会显得大一圈。
#[allow(dead_code)]
const CONTENT_SCALE: f64 = 0.80;

/// 打包图标的原始 PNG 字节（运行时解码一次）。
#[allow(dead_code)]
const ICON_PNG: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/icon.png"));

/// 会话监视报告的「是否存在运行中会话」。
#[cfg(target_os = "macos")]
static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 会话监视在每次轮询后调用：报告当前是否存在运行中的会话。
pub(crate) fn set_running(active: bool) {
    #[cfg(target_os = "macos")]
    RUNNING.store(active, std::sync::atomic::Ordering::Release);
    #[cfg(not(target_os = "macos"))]
    let _ = active;
}

/// 应用启动时初始化（setup 钩子调用一次）。非 macOS 平台为 no-op。
pub(crate) fn init(app: tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let Some(engine) = Engine::load() else {
            // 图标解码失败：该功能不可用，仅记录一条日志后放弃。
            crate::service::emit_log(&app, "dock", "warning", "Dock 图标解码失败，渐变指示不可用");
            return;
        };
        if !store_engine(engine) {
            return; // 已初始化过（防御性；init 只应被调用一次）
        }
        if std::thread::Builder::new().name("dock-frames".into()).spawn(move || {
            // 兜底：任何意外 panic 只记一条日志，并恢复原始图标，不影响宿主。
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                imp::cycle_loop(app.clone());
            }));
            if result.is_err() {
                crate::service::emit_log(&app, "dock", "error", "Dock 指示：意外异常退出，已恢复原始图标");
            }
            imp::restore_base(&app);
        })
        .is_err()
        {
            // 线程创建失败：彻底放弃，不影响任何功能。
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

/// 渐变引擎：常驻降采样 base 像素与按钮掩码，按需即时生成单帧 PNG。
#[allow(dead_code)]
struct Engine {
    /// 工作分辨率（正方形边长）。
    size: usize,
    /// 降采样后的 base RGBA。
    base_rgba: Vec<u8>,
    /// 电源按钮圆盘的像素索引（右下象限内绿色主导的像素）。
    indices: Vec<usize>,
}

#[allow(dead_code)]
impl Engine {
    /// 解码打包图标 → 降采样到工作分辨率 → 定位按钮掩码；失败返回 None。
    fn load() -> Option<Self> {
        let image = tauri::image::Image::from_bytes(ICON_PNG).ok()?;
        let (mut width, mut height, mut rgba) =
            (image.width() as usize, image.height() as usize, image.rgba().to_vec());
        while width.min(height) > WORK_SIZE_LIMIT {
            rgba = downsample_half(&rgba, width, height);
            width /= 2;
            height /= 2;
        }
        if width == 0 || rgba.len() < width * height * 4 {
            return None;
        }
        // 按系统图标栅格补足留白：直接铺满的 artwork 会比邻居图标大一圈。
        rgba = embed_scaled(&rgba, width, CONTENT_SCALE);
        let indices = locate_button(&rgba, width, height);
        Some(Engine { size: width, base_rgba: rgba, indices })
    }

    /// 生成第 `phase`（0..1，0=原始绿，1=完全红）帧的 PNG 字节。
    #[allow(dead_code)]
    fn frame_png(&self, phase: f64) -> Vec<u8> {
        let mut rgba = self.base_rgba.clone();
        recolor(&mut rgba, &self.indices, phase);
        encode_png(&rgba, self.size, self.size)
    }

    /// base 图标的 PNG 字节（恢复原始图标时使用）。
    fn base_png(&self) -> Vec<u8> {
        encode_png(&self.base_rgba, self.size, self.size)
    }
}

/// 把正方形图像（内容铺满）按 `ratio` 等比缩小后居中置入同尺寸画布。
/// 用于给 artwork 补充系统图标栅格的标准留白；双线性采样避免缩放锯齿。
fn embed_scaled(rgba: &[u8], size: usize, ratio: f64) -> Vec<u8> {
    let content = ((size as f64 * ratio).round() as usize).clamp(1, size);
    if content >= size {
        return rgba.to_vec();
    }
    let scaled = resize_bilinear(rgba, size, size, content, content);
    let offset = (size - content) / 2;
    let mut out = vec![0_u8; size * size * 4];
    for y in 0..content {
        let dst = ((y + offset) * size + offset) * 4;
        out[dst..dst + content * 4].copy_from_slice(&scaled[y * content * 4..(y + 1) * content * 4]);
    }
    out
}

/// 双线性缩放 RGBA 图像（含 alpha）。
fn resize_bilinear(rgba: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0_u8; dw * dh * 4];
    for y in 0..dh {
        let gy = (y as f64 + 0.5) * sh as f64 / dh as f64 - 0.5;
        let y0 = gy.floor().max(0.0) as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let fy = (gy - y0 as f64).clamp(0.0, 1.0);
        for x in 0..dw {
            let gx = (x as f64 + 0.5) * sw as f64 / dw as f64 - 0.5;
            let x0 = gx.floor().max(0.0) as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let fx = (gx - x0 as f64).clamp(0.0, 1.0);
            for c in 0..4 {
                let p00 = rgba[(y0 * sw + x0) * 4 + c] as f64;
                let p10 = rgba[(y0 * sw + x1) * 4 + c] as f64;
                let p01 = rgba[(y1 * sw + x0) * 4 + c] as f64;
                let p11 = rgba[(y1 * sw + x1) * 4 + c] as f64;
                let top = p00 * (1.0 - fx) + p10 * fx;
                let bottom = p01 * (1.0 - fx) + p11 * fx;
                out[(y * dw + x) * 4 + c] =
                    (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// 2×2 块降采样（RGB 按 alpha 加权平均，避免透明像素把边缘拖出暗色）。
#[allow(dead_code)]
fn downsample_half(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    let (out_w, out_h) = (width / 2, height / 2);
    let mut out = vec![0_u8; out_w * out_h * 4];
    for y in 0..out_h {
        for x in 0..out_w {
            let (mut sums, mut alpha_sum) = ([0_f64; 3], 0_f64);
            for dy in 0..2 {
                for dx in 0..2 {
                    let index = (((2 * y + dy) * width) + (2 * x + dx)) * 4;
                    let alpha = rgba[index + 3] as f64;
                    for channel in 0..3 {
                        sums[channel] += rgba[index + channel] as f64 * alpha;
                    }
                    alpha_sum += alpha;
                }
            }
            let out_index = (y * out_w + x) * 4;
            let average_alpha = alpha_sum / 4.0;
            if average_alpha > 0.0 {
                for channel in 0..3 {
                    out[out_index + channel] = (sums[channel] / alpha_sum).round().clamp(0.0, 255.0) as u8;
                }
            }
            out[out_index + 3] = average_alpha.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// 识别右下象限内绿色主导的电源按钮圆盘像素（返回像素索引列表）。
#[allow(dead_code)]
///
/// 掩码规则：不透明或半透明、g 通道显著高于 r/b 的像素；找不到时回退到
/// 几何默认位置（0.73 边长处半径 0.12）的圆盘，保证指示仍可见。
fn locate_button(rgba: &[u8], width: usize, height: usize) -> Vec<usize> {
    let is_button_green = |r: u8, g: u8, b: u8, a: u8| {
        a > 60 && g > 110 && g as u16 > r as u16 + 14 && g as u16 > b as u16 + 10
    };
    let x_floor = width / 2;
    let y_floor = height / 2;
    let mut indices = Vec::new();
    for y in y_floor..height {
        for x in x_floor..width {
            let index = (y * width + x) * 4;
            if is_button_green(rgba[index], rgba[index + 1], rgba[index + 2], rgba[index + 3]) {
                indices.push(index);
            }
        }
    }
    if !indices.is_empty() {
        return indices;
    }
    // 几何回退：圆盘近似为以 (0.73, 0.73) 边长为中心、半径 0.12 的实心圆。
    let min_side = width.min(height) as f64;
    let (center, radius) = (0.73 * min_side, 0.12 * min_side);
    for y in y_floor..height {
        for x in x_floor..width {
            let dx = x as f64 - center;
            let dy = y as f64 - center;
            if (dx * dx + dy * dy).sqrt() <= radius {
                indices.push((y * width + x) * 4);
            }
        }
    }
    indices
}

/// 把按钮圆盘像素重着色为「绿 → 红」渐变的第 `phase`（0..1）帧。
#[allow(dead_code)]
///
/// 颜色插值在 HSL 色相轴上进行：按钮绿 ≈ 134°，红色 = 0°；
/// phase=0 不动，phase=1 完全转红；饱和度保持、亮度轻微下降（红更沉稳）。
fn recolor(rgba: &mut [u8], indices: &[usize], phase: f64) {
    if phase <= 0.0 {
        return;
    }
    let hue_shift_deg = 140.0 * phase.clamp(0.0, 1.0);
    let dimming = 1.0 - 0.12 * phase.clamp(0.0, 1.0);
    for &index in indices {
        let (r, g, b) = (rgba[index] as f64, rgba[index + 1] as f64, rgba[index + 2] as f64);
        let (hue, saturation, lightness) = rgb_to_hsl(r, g, b);
        let shifted_hue = (hue - hue_shift_deg).rem_euclid(360.0);
        let shifted_lightness = lightness * dimming;
        let (nr, ng, nb) = hsl_to_rgb(shifted_hue, saturation, shifted_lightness);
        rgba[index] = nr;
        rgba[index + 1] = ng;
        rgba[index + 2] = nb;
    }
}

/// RGB(0..255) → HSL（h: 0..360）。
#[allow(dead_code)]
fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    if (max - min).abs() < f64::EPSILON {
        return (0.0, 0.0, lightness);
    }
    let delta = max - min;
    let saturation = if lightness > 0.5 { delta / (2.0 - max - min) } else { delta / (max + min) };
    let hue = if (max - r).abs() < f64::EPSILON {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if (max - g).abs() < f64::EPSILON {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (hue, saturation, lightness)
}

/// HSL → RGB(0..255 取整)。
#[allow(dead_code)]
fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> (u8, u8, u8) {
    if saturation.abs() < f64::EPSILON {
        let value = (lightness * 255.0).round().clamp(0.0, 255.0) as u8;
        return (value, value, value);
    }
    let q = if lightness < 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let p = 2.0 * lightness - q;
    let channel = |mut t: f64| {
        t = (t % 360.0 + 360.0) % 360.0 / 360.0;
        let value = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (value * 255.0).round().clamp(0.0, 255.0) as u8
    };
    (channel(hue + 120.0), channel(hue), channel(hue - 120.0))
}

/// 最小 PNG 编码（RGBA8、filter 0、zlib 流），供 Dock 图标帧使用。
/// flate2 已在依赖中，无需额外引入图像编码器。
#[allow(dead_code)]
fn encode_png(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    use std::io::Write as _;

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFF_u32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }
    fn append_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    // 8bit depth, RGBA color type, deflate, adaptive filters, no interlace.
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    append_chunk(&mut png, b"IHDR", &ihdr);
    // 每行前置 filter byte 0（None）。
    let mut raw = Vec::with_capacity(rgba.len() + height * 4);
    for y in 0..height {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * width * 4..(y + 1) * width * 4]);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = encoder.write_all(&raw);
    append_chunk(&mut png, b"IDAT", &encoder.finish().unwrap_or_default());
    append_chunk(&mut png, b"IEND", &[]);
    png
}

// —— macOS 专属：Dock 图标帧循环 ——

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::OnceLock;
    use std::sync::atomic::Ordering;
    use std::thread;

    use super::{Engine, FRAME_PERIOD, RUNNING};

    static ENGINE: OnceLock<Engine> = OnceLock::new();

    /// 有运行会话时循环播放闪烁帧；无会话时恢复原始图标（只恢复一次）。
    pub(super) fn cycle_loop(app: tauri::AppHandle) {
        #[derive(PartialEq)]
        enum Shown {
            Original,
            Animated,
        }
        let Some(engine) = ENGINE.get() else { return };
        // TODO(临时测试)：忽略会话状态，启动即闪烁；确认后改回 false。—— 已确认，恢复正常联动
        const FORCE_BLINK: bool = false;
        // 工作状态闪烁：绿色（空闲底色）与红色（工作色）两态直接交替，
        // 无中间渐变帧——语义为“工作指示灯在闪烁”。
        let green = engine.frame_png(0.0);
        let red = engine.frame_png(1.0);
        let mut shown = Shown::Original;
        let mut red_active = false;
        loop {
            thread::sleep(FRAME_PERIOD);
            if FORCE_BLINK || RUNNING.load(Ordering::Acquire) {
                let png = if red_active { &red } else { &green };
                apply_dock_image(&app, png.clone());
                red_active = !red_active;
                shown = Shown::Animated;
            } else if shown == Shown::Animated {
                // 恢复原始图标。
                if red_active {
                    apply_dock_image(&app, green.clone());
                }
                shown = Shown::Original;
            }
        }
    }

    /// `setApplicationIconImage` 是主线程专用 API，经 tauri 事件循环派发到主线程。
    ///
    /// 失败只静默丢弃当前帧（下一帧会重试）；成功不打日志，避免高频刷屏。
    fn apply_dock_image(app: &tauri::AppHandle, png: Vec<u8>) {
        let _ = app.clone().run_on_main_thread(move || unsafe {
            use objc2::{AllocAnyThread, MainThreadMarker};
            use objc2_app_kit::{NSApplication, NSImage};
            use objc2_foundation::NSData;

            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let data = NSData::with_bytes(&png);
            if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
                let application = NSApplication::sharedApplication(mtm);
                application.setApplicationIconImage(Some(&image));
            }
        });
    }

    pub(super) fn store_engine(engine: Engine) -> bool {
        ENGINE.set(engine).is_ok()
    }

    /// 恢复原始 Dock 图标（动画线程异常退出时防“卡在中间帧”）。
    pub(super) fn restore_base(app: &tauri::AppHandle) {
        if let Some(engine) = ENGINE.get() {
            apply_dock_image(app, engine.base_png());
        }
    }
}

#[cfg(target_os = "macos")]
fn store_engine(engine: Engine) -> bool {
    imp::store_engine(engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成一张 64x64 测试图：右下角一个绿色圆盘（模拟电源按钮），其余透明。
    fn sample_rgba() -> (usize, usize, Vec<u8>) {
        let (w, h) = (64usize, 64usize);
        let mut rgba = vec![0_u8; w * h * 4];
        let center = (47.0_f64, 47.0_f64); // 0.73 * 64 ≈ 47
        let radius = 10.0_f64;
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - center.0;
                let dy = y as f64 - center.1;
                if (dx * dx + dy * dy).sqrt() <= radius {
                    let index = (y * w + x) * 4;
                    rgba[index] = 63;
                    rgba[index + 1] = 163;
                    rgba[index + 2] = 87;
                    rgba[index + 3] = 255;
                }
            }
        }
        (w, h, rgba)
    }

    #[test]
    fn recolor_moves_hue_toward_red() {
        let (width, height, mut rgba) = sample_rgba();
        let button_index = (47 * width + 47) * 4;
        let indices = locate_button(&rgba, width, height);
        let original_g = rgba[button_index + 1];
        recolor(&mut rgba, &indices, 1.0); // phase=1：色相偏移最大
        let shifted_g = rgba[button_index + 1];
        assert!(
            shifted_g < original_g,
            "phase=1 时按钮绿色分量应下降（{original_g} → {shifted_g}）"
        );
        // 透明区域不受影响。
        assert_eq!(rgba[0], 0);
        assert_eq!(rgba[3], 0);
    }

    #[test]
    fn recolor_phase_zero_is_noop() {
        let (width, height, mut rgba) = sample_rgba();
        let base = rgba.clone();
        let indices = locate_button(&rgba, width, height);
        recolor(&mut rgba, &indices, 0.0);
        assert_eq!(base, rgba, "phase=0 应保持原始颜色");
    }

    #[test]
    fn recolor_survives_empty_mask() {
        // 全透明图像：掩码为空（locate 的几何回退索引指向透明像素也安全）。
        let mut rgba = vec![0_u8; 64 * 64 * 4];
        let indices = locate_button(&rgba, 64, 64);
        recolor(&mut rgba, &indices, 0.5);
    }

    #[test]
    fn locate_button_finds_disc_on_real_icon() {
        let image = tauri::image::Image::from_bytes(ICON_PNG).expect("图标可解码");
        let rgba = image.rgba().to_vec();
        let indices = locate_button(&rgba, 512, 512);
        assert!(
            indices.len() > 1000,
            "真实图标电源按钮掩码应命中大量像素（{}）",
            indices.len()
        );
        // 全部命中右下象限。
        for &index in &indices {
            let pixel = index / 4;
            assert!(pixel / 512 >= 256 && pixel % 512 >= 256);
        }
    }

    #[test]
    fn encode_png_produces_valid_stream() {
        let (width, height, rgba) = sample_rgba();
        let png = encode_png(&rgba, width, height);
        assert!(png.len() > 8);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert!(png.windows(4).any(|w| w == b"IDAT"));
        assert!(png.windows(4).any(|w| w == b"IEND"));
    }

    #[test]
    fn downsample_half_keeps_button_visible() {
        let (w, h, rgba) = sample_rgba();
        let half = downsample_half(&rgba, w, h);
        // 降采样后仍存在绿色主导的像素（按钮不会被平均掉）。
        let hits = half
            .chunks(4)
            .filter(|p| p[3] > 60 && p[1] > 110 && p[1] as u16 > p[0] as u16 + 14)
            .count();
        assert!(hits > 0, "降采样不应丢失绿色按钮");
    }

    #[test]
    fn engine_frame_png_matches_phase_semantics() {
        let Some(engine) = Engine::load() else { panic!("打包图标应能解码") };
        assert_eq!(engine.size, 256, "引擎应降采样到工作分辨率上限");
        let base_png = engine.frame_png(0.0);
        assert_eq!(base_png, engine.base_png(), "phase=0 帧应与 base 图标完全一致");
        let mid = engine.frame_png(1.0);
        assert_ne!(mid, base_png, "phase=1 帧应与原始图标不同（电源按钮已转红）");
    }

    #[test]
    fn hsl_round_trip_preserves_green_hue() {
        let (hue, saturation, lightness) = rgb_to_hsl(63.0, 163.0, 87.0);
        assert!(
            (120.0..150.0).contains(&hue),
            "电源按钮绿色色相应在绿区（实际 {hue}）"
        );
        assert!(saturation > 0.4);
        let (r, g, b) = hsl_to_rgb(hue, saturation, lightness);
        assert!(
            (f64::from(r) - 63.0).abs() <= 1.0
                && (f64::from(g) - 163.0).abs() <= 1.0
                && (f64::from(b) - 87.0).abs() <= 1.0,
            "HSL 往返应还原原始色（{r} {g} {b}）"
        );
    }




    /// 临时诊断：输出补留白后的红帧 PNG。
    /// `cargo test --lib dump_padded -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_padded() {
        let Some(engine) = Engine::load() else { panic!("解码失败") };
        std::fs::write("/tmp/dsh-dock-padded-red.png", engine.frame_png(1.0)).unwrap();
        println!("written /tmp/dsh-dock-padded-red.png ({} 字节)", engine.indices.len());
    }
}
