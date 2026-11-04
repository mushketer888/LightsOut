use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use resvg::{tiny_skia, usvg};

mod build_git;

const ICON_SVG: &str = "assets/icon.svg";
const TRAY_SVG: &str = "assets/tray.svg";
const ICO_SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];
/// Tray sizes; 20 covers 125% DPI. Grid-fitted in assets/tray.svg for 16-32 px.
const TRAY_ICO_SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];

/// Days since 1970-01-01 -> (year, month, day), Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn render(tree: &usvg::Tree, size: u32) -> tiny_skia::Pixmap {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("non-zero icon size");
    let scale = size as f32 / tree.size().width();
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
}

/// ICO container with PNG-compressed entries (supported since Windows Vista).
fn ico_from(images: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]); // reserved, type = icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, png) in &images {
        out.push(if *size >= 256 { 0 } else { *size as u8 }); // width, 0 = 256
        out.push(if *size >= 256 { 0 } else { *size as u8 }); // height
        out.extend_from_slice(&[0, 0]); // palette, reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &images {
        out.extend_from_slice(png);
    }
    out
}

fn tree_images(tree: &usvg::Tree, sizes: &[u32]) -> Vec<(u32, Vec<u8>)> {
    sizes
        .iter()
        .map(|&s| (s, render(tree, s).encode_png().expect("PNG encode")))
        .collect()
}

/// Renders icon.svg into `<OUT_DIR>/icon.ico` (the app icon: exe, installer, README).
fn build_icons(out_dir: &Path) -> PathBuf {
    let svg = std::fs::read(ICON_SVG).expect("assets/icon.svg");
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("valid SVG");
    let ico_path = out_dir.join("icon.ico");
    std::fs::write(&ico_path, ico_from(tree_images(&tree, &ICO_SIZES))).expect("write icon.ico");
    ico_path
}

/// Pause badge in the bottom-right corner: a dark disc with two white bars.
fn draw_pause_badge(pm: &mut tiny_skia::Pixmap) {
    badge_disc(pm, |fx, fy, r| {
        fy.abs() <= r * 0.5
            && ((-r * 0.52..=-r * 0.16).contains(&fx) || (r * 0.16..=r * 0.52).contains(&fx))
    });
}

/// Snooze badge: a dark disc with a white hourglass — two triangles meeting
/// at the centre (base 0.8r at the top/bottom, tip in the middle).
fn draw_snooze_badge(pm: &mut tiny_skia::Pixmap) {
    badge_disc(pm, |fx, fy, r| {
        let half = fy.abs() / (0.5 * r); // 0 at the middle, 1 at the base
        fy.abs() <= 0.5 * r && fx.abs() <= 0.4 * r * half
    });
}

/// Paints a dark badge disc at (0.69, 0.69) with radius 0.30; `white` decides
/// which pixels inside the disc are white, the rest stay dark.
fn badge_disc(pm: &mut tiny_skia::Pixmap, white: impl Fn(f32, f32, f32) -> bool) {
    let n = pm.width();
    let nf = n as f32;
    let (cx, cy, r) = (nf * 0.69, nf * 0.69, nf * 0.30);
    let data = pm.data_mut();
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            if fx.hypot(fy) > r {
                continue;
            }
            let px: [u8; 4] = if white(fx, fy, r) {
                [255, 255, 255, 255]
            } else {
                [24, 24, 28, 255]
            };
            let i = ((y * n + x) * 4) as usize;
            data[i..i + 4].copy_from_slice(&px);
        }
    }
}

/// Renders tray.svg + a badge into an ICO with one entry per DPI size.
/// Badges are baked per size instead of drawn at runtime, so every variant
/// ships pre-scaled and Windows picks the crispest entry.
fn tray_ico(tree: &usvg::Tree, paused: bool, snoozed: bool) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = TRAY_ICO_SIZES
        .iter()
        .map(|&s| {
            let mut pm = render(tree, s);
            if paused {
                draw_pause_badge(&mut pm);
            }
            if snoozed {
                draw_snooze_badge(&mut pm);
            }
            (s, pm.encode_png().expect("PNG encode"))
        })
        .collect();
    ico_from(images)
}

/// Renders tray.svg into `<OUT_DIR>/tray*.ico` (normal, paused, snoozed).
fn build_tray_icons(out_dir: &Path) {
    let svg = std::fs::read(TRAY_SVG).expect("assets/tray.svg");
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("valid SVG");
    for (name, paused, snoozed) in [
        ("tray.ico", false, false),
        ("tray-paused.ico", true, false),
        ("tray-snoozed.ico", false, true),
    ] {
        std::fs::write(out_dir.join(name), tray_ico(&tree, paused, snoozed))
            .expect("write tray ico");
    }
}

fn main() {
    let git = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs / 86_400);

    println!("cargo:rustc-env=LIGHTS_OUT_GIT={git}");
    println!("cargo:rustc-env=LIGHTS_OUT_BUILD_DATE={y:04}-{m:02}-{d:02}");
    for path in build_git::watch_paths(Path::new(".")) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-changed=build_git.rs");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON_SVG}");
    println!("cargo:rerun-if-changed={TRAY_SVG}");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    #[cfg_attr(not(windows), allow(unused_variables))]
    let ico_path = build_icons(&out_dir);
    #[cfg_attr(not(windows), allow(unused_variables))]
    let tray_paths: [&str; 3] = ["tray.ico", "tray-paused.ico", "tray-snoozed.ico"];
    build_tray_icons(&out_dir);

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "LightsOut");
        res.set("FileDescription", "LightsOut idle auto-shutdown");
        res.set_icon(&ico_path.to_string_lossy());
        // Tray variants as resources 2-4; tray-icon loads them with Icon::from_resource.
        for (i, name) in tray_paths.iter().enumerate() {
            res.set_icon_with_id(
                &out_dir.join(name).to_string_lossy(),
                &(2 + i as u16).to_string(),
            );
        }
        res.compile()
            .expect("compile mandatory Windows icon resources");
    }
}
