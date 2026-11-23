//! Window geometry and layout defaults for the desktop shell.
//!
//! Sizes are data, not literals: a user-editable JSON file (or
//! `PRINTKIT_LAYOUT_FILE`) overrides the defaults, and the last size/position
//! of every window is written back. The defaults are the compact/split
//! breakpoints the UI is designed around, not a hard-coded contract.
//!
//! The pure functions ([`clamp_to_work_area`], [`restore_or_default`],
//! [`maximized_keeps_restore_size`]) are the testable core: the Tauri layer
//! only feeds them monitor rectangles and window geometry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Monitor};

/// Smallest inner size shared by every managed window.
pub const MIN_WIDTH: f64 = 560.0;
pub const MIN_HEIGHT: f64 = 420.0;

/// Default business window size; 900×633 keeps the 1024-wide sidebar layout
/// inside a compact 16:11.25 window.
pub const DEFAULT_BUSINESS_WIDTH: f64 = 900.0;
pub const DEFAULT_BUSINESS_HEIGHT: f64 = 633.0;

/// Saved-layout schema version. Version 4 resets the 1024×720 default once.
pub const LAYOUT_VERSION: u32 = 4;

pub const LAYOUT_FILE: &str = "desktop-window.json";
const SAVE_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Geometry {
    pub width: f64,
    pub height: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    /// Maximized windows keep the restore size in `width`/`height`.
    #[serde(default)]
    pub maximized: bool,
}

impl Geometry {
    fn at(width: f64, height: f64, x: Option<f64>, y: Option<f64>) -> Self {
        Self {
            width,
            height,
            x,
            y,
            maximized: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    /// Layout schema version; files from before S5 omit this and read as 0.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub windows: BTreeMap<String, Geometry>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            version: LAYOUT_VERSION,
            windows: BTreeMap::new(),
        }
    }
}

/// Default (logical) size for a window label.
#[derive(Debug, Clone)]
pub struct WindowSpec {
    pub width: f64,
    pub height: f64,
    pub min_width: f64,
    pub min_height: f64,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub maximized: bool,
}

/// A logical-pixel rectangle in virtual-screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A monitor's usable area (logical pixels, negative coordinates on macOS).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkArea {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl WorkArea {
    fn intersection(&self, rect: &Rect) -> f64 {
        let left = self.x.max(rect.x);
        let right = (self.x + self.width).min(rect.x + rect.width);
        let top = self.y.max(rect.y);
        let bottom = (self.y + self.height).min(rect.y + rect.height);
        (right - left).max(0.0) * (bottom - top).max(0.0)
    }
}

/// Defaults for the single business window.
pub fn default_spec(label: &str) -> Option<WindowSpec> {
    if label != "business" {
        return None;
    }
    Some(WindowSpec {
        width: DEFAULT_BUSINESS_WIDTH,
        height: DEFAULT_BUSINESS_HEIGHT,
        min_width: MIN_WIDTH,
        min_height: MIN_HEIGHT,
        x: None,
        y: None,
        maximized: false,
    })
}

/// Pick the work area that covers most of the window; a window restored
/// from a display that no longer exists falls back to the primary area.
fn choose_area(rect: &Rect, areas: &[WorkArea]) -> Option<WorkArea> {
    let mut best: Option<(f64, WorkArea)> = None;
    for area in areas {
        let overlap = area.intersection(rect);
        if best.is_none() || overlap > best.as_ref().map_or(0.0, |(value, _)| *value) {
            best = Some((overlap, *area));
        }
    }
    match best {
        Some((overlap, area)) if overlap > 0.0 => Some(area),
        // Completely off-screen: the first area is the primary monitor.
        _ => areas.first().copied(),
    }
}

/// Clamp a window rectangle so it is fully visible on a work area. Negative
/// coordinates (secondary displays left/above the primary) are preserved.
pub fn clamp_to_work_area(rect: Rect, areas: &[WorkArea]) -> Rect {
    let Some(area) = choose_area(&rect, areas) else {
        return rect;
    };
    let width = if area.width <= MIN_WIDTH {
        area.width
    } else {
        rect.width.clamp(MIN_WIDTH, area.width)
    };
    let height = if area.height <= MIN_HEIGHT {
        area.height
    } else {
        rect.height.clamp(MIN_HEIGHT, area.height)
    };
    let x = if width >= area.width {
        area.x
    } else {
        rect.x.clamp(area.x, area.x + area.width - width)
    };
    let y = if height >= area.height {
        area.y
    } else {
        rect.y.clamp(area.y, area.y + area.height - height)
    };
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// A maximized window reports the maximized size; the last floating size and
/// position must survive so un-maximizing returns where the user left it.
pub fn maximized_keeps_restore_size(
    previous: Option<&Geometry>,
    current: Rect,
    maximized: bool,
) -> Rect {
    match (maximized, previous) {
        (true, Some(previous)) => Rect {
            x: previous.x.unwrap_or(current.x),
            y: previous.y.unwrap_or(current.y),
            width: previous.width.max(MIN_WIDTH),
            height: previous.height.max(MIN_HEIGHT),
        },
        _ => current,
    }
}

/// Saved geometry wins, but never below the shared minimum and never
/// off-screen. A missing or unreadable entry returns the default spec.
pub fn restore_or_default(label: &str, layout: &Layout, areas: &[WorkArea]) -> Option<WindowSpec> {
    let mut spec = default_spec(label)?;
    let Some(geometry) = layout.windows.get(label) else {
        // Defaults larger than the screen are shrunk to the primary work area.
        if let Some(area) = areas.first() {
            spec.width = spec.width.clamp(spec.min_width, area.width.max(spec.min_width));
            spec.height = spec.height.clamp(spec.min_height, area.height.max(spec.min_height));
        }
        return Some(spec);
    };
    spec.width = geometry.width.max(MIN_WIDTH);
    spec.height = geometry.height.max(MIN_HEIGHT);
    spec.maximized = geometry.maximized;
    if let (Some(x), Some(y)) = (geometry.x, geometry.y) {
        let rect = clamp_to_work_area(
            Rect {
                x,
                y,
                width: spec.width,
                height: spec.height,
            },
            areas,
        );
        spec.x = Some(rect.x);
        spec.y = Some(rect.y);
        spec.width = rect.width;
        spec.height = rect.height;
    }
    Some(spec)
}

fn logical_work_area(monitor: &Monitor) -> WorkArea {
    let scale = if monitor.scale_factor() > 0.0 {
        monitor.scale_factor()
    } else {
        1.0
    };
    let position = monitor.work_area().position;
    let size = monitor.work_area().size;
    WorkArea {
        x: f64::from(position.x) / scale,
        y: f64::from(position.y) / scale,
        width: f64::from(size.width) / scale,
        height: f64::from(size.height) / scale,
    }
}

/// Work areas in logical pixels, primary first.
pub fn work_areas(app: &AppHandle) -> Vec<WorkArea> {
    let mut areas = Vec::new();
    if let Ok(Some(primary)) = app.primary_monitor() {
        areas.push(logical_work_area(&primary));
    }
    if let Ok(monitors) = app.available_monitors() {
        for monitor in monitors {
            let area = logical_work_area(&monitor);
            if !areas.contains(&area) {
                areas.push(area);
            }
        }
    }
    areas
}

fn is_old_business_default(width: f64, height: f64) -> bool {
    [
        (1024.0, 720.0),
        (1024.0, 640.0),
        (560.0, 420.0),
        (660.0, 600.0),
        (663.0, 458.0),
    ]
    .iter()
    .any(|(old_width, old_height)| {
        (width - old_width).abs() < 0.5 && (height - old_height).abs() < 0.5
    })
}

/// One-time schema migration. Only exact old defaults are reset; a user's
/// custom width/height survives, and x/y/maximized state is always preserved.
fn migrate_layout(layout: &mut Layout) -> bool {
    if layout.version >= LAYOUT_VERSION {
        return false;
    }
    if let Some(geometry) = layout.windows.get_mut("business") {
        if is_old_business_default(geometry.width, geometry.height) {
            geometry.width = DEFAULT_BUSINESS_WIDTH;
            geometry.height = DEFAULT_BUSINESS_HEIGHT;
        }
    }
    layout.version = LAYOUT_VERSION;
    true
}

fn load_layout_file(path: &Path) -> Layout {
    let mut layout = read_layout(path);
    if migrate_layout(&mut layout) {
        let _ = write_layout(path, &layout);
    }
    layout
}

pub fn layout_path(app: &AppHandle) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("PRINTKIT_LAYOUT_FILE") {
        return Some(PathBuf::from(value));
    }
    app.path()
        .app_config_dir()
        .ok()
        .map(|directory| directory.join(LAYOUT_FILE))
}

pub fn read_layout(path: &Path) -> Layout {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn write_layout(path: &Path, layout: &Layout) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create the layout directory: {error}"))?;
    }
    let text = serde_json::to_string_pretty(layout)
        .map_err(|error| format!("cannot serialize the layout: {error}"))?;
    std::fs::write(path, text).map_err(|error| format!("cannot write the layout: {error}"))
}

pub fn load(app: &AppHandle) -> Layout {
    layout_path(app)
        .map(|path| load_layout_file(&path))
        .unwrap_or_default()
}

fn save_index() -> std::sync::MutexGuard<'static, (BTreeMap<String, Instant>, BTreeMap<String, bool>)>
{
    static SAVE: Mutex<(BTreeMap<String, Instant>, BTreeMap<String, bool>)> =
        Mutex::new((BTreeMap::new(), BTreeMap::new()));
    SAVE.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Capture logical inner size/position and the maximized flag. Called from
/// `Resized`/`Moved`; writes are throttled so dragging does not rewrite the
/// JSON on every pixel, and the final event is flushed by a delayed save.
pub fn remember_window(window: &tauri::Window) {
    let label = window.label().to_owned();
    let now = Instant::now();
    let throttled = {
        let (last, _) = &mut *save_index();
        match last.get(&label) {
            Some(at) if now.duration_since(*at) < SAVE_INTERVAL => true,
            _ => {
                last.insert(label.clone(), now);
                false
            }
        }
    };
    if throttled {
        schedule_flush(window.clone());
    } else {
        save_window(window);
    }
}

/// Immediately persist, bypassing the throttle (window close, app quit).
pub fn remember_window_now(window: &tauri::Window) {
    save_window(window);
}

fn schedule_flush(window: tauri::Window) {
    let label = window.label().to_owned();
    {
        let (_, pending) = &mut *save_index();
        if pending.contains_key(&label) {
            return;
        }
        pending.insert(label.clone(), true);
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(SAVE_INTERVAL).await;
        save_window(&window);
        let (_, pending) = &mut *save_index();
        pending.remove(&label);
    });
}

fn save_window(window: &tauri::Window) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let Ok(size) = window.inner_size() else {
        return;
    };
    let Ok(position) = window.outer_position() else {
        return;
    };
    let maximized = window.is_maximized().unwrap_or(false);
    let current = Rect {
        x: f64::from(position.x) / scale,
        y: f64::from(position.y) / scale,
        width: f64::from(size.width) / scale,
        height: f64::from(size.height) / scale,
    };
    let Some(path) = layout_path(window.app_handle()) else {
        return;
    };
    let mut layout = read_layout(&path);
    let previous = layout.windows.get(window.label()).cloned();
    let kept = maximized_keeps_restore_size(previous.as_ref(), current, maximized);
    let entry = layout
        .windows
        .entry(window.label().to_owned())
        .or_insert(Geometry::at(kept.width, kept.height, None, None));
    entry.width = kept.width;
    entry.height = kept.height;
    entry.x = Some(kept.x);
    entry.y = Some(kept.y);
    entry.maximized = maximized;
    let _ = write_layout(&path, &layout);
}

/// Restore the business window created by `tauri.conf.json` (the other
/// windows are restored when they are built from their spec).
pub fn apply_saved(app: &AppHandle) {
    let layout = load(app);
    let areas = work_areas(app);
    let Some(window) = app.get_webview_window("business") else {
        return;
    };
    let Some(spec) = restore_or_default("business", &layout, &areas) else {
        return;
    };
    let _ = window.set_size(tauri::LogicalSize::new(spec.width, spec.height));
    if let (Some(x), Some(y)) = (spec.x, spec.y) {
        let _ = window.set_position(tauri::LogicalPosition::new(x, y));
    }
    if spec.maximized {
        let _ = window.maximize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "printkit-layout-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    fn area(x: f64, y: f64, width: f64, height: f64) -> WorkArea {
        WorkArea {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn every_window_has_the_shared_560x420_minimum() {
        let business = default_spec("business").unwrap();
        assert_eq!(business.min_width, 560.0);
        assert_eq!(business.min_height, 420.0);
        assert!(business.min_width <= business.width);
        assert!(business.min_height <= business.height);
        assert_eq!((business.width, business.height), (900.0, 633.0));
        for label in ["records", "printers", "dev", "unknown"] {
            assert!(default_spec(label).is_none(), "{label}");
        }
    }

    #[test]
    fn saved_geometry_overrides_defaults_and_clamps_to_minimum() {
        let mut layout = Layout::default();
        layout.windows.insert(
            "business".to_owned(),
            Geometry::at(120.0, 100.0, Some(40.0), Some(60.0)),
        );
        let areas = [area(0.0, 0.0, 1440.0, 900.0)];
        let spec = restore_or_default("business", &layout, &areas).unwrap();
        assert_eq!((spec.width, spec.height), (560.0, 420.0));
        assert_eq!((spec.x, spec.y), (Some(40.0), Some(60.0)));
    }

    #[test]
    fn clamp_keeps_a_window_fully_inside_the_work_area() {
        let areas = [area(0.0, 0.0, 1440.0, 900.0)];
        let rect = clamp_to_work_area(
            Rect {
                x: 1200.0,
                y: 700.0,
                width: 800.0,
                height: 500.0,
            },
            &areas,
        );
        assert_eq!(rect.x, 640.0);
        assert_eq!(rect.y, 400.0);
        assert_eq!((rect.width, rect.height), (800.0, 500.0));
    }

    #[test]
    fn clamp_accepts_negative_coordinates_on_a_secondary_display() {
        let areas = [
            area(0.0, 0.0, 1440.0, 900.0),
            area(-2560.0, -200.0, 2560.0, 1440.0),
        ];
        let rect = clamp_to_work_area(
            Rect {
                x: -2200.0,
                y: -100.0,
                width: 900.0,
                height: 600.0,
            },
            &areas,
        );
        assert_eq!((rect.x, rect.y), (-2200.0, -100.0));
    }

    #[test]
    fn a_window_on_a_removed_display_is_pulled_back_to_the_primary() {
        let areas = [area(0.0, 0.0, 1440.0, 900.0)];
        let rect = clamp_to_work_area(
            Rect {
                x: 3000.0,
                y: 120.0,
                width: 760.0,
                height: 600.0,
            },
            &areas,
        );
        assert_eq!(rect.x, 680.0);
        assert_eq!(rect.y, 120.0);
    }

    #[test]
    fn restore_pulls_an_offscreen_window_back() {
        let mut layout = Layout::default();
        layout.windows.insert(
            "business".to_owned(),
            Geometry::at(960.0, 680.0, Some(4000.0), Some(200.0)),
        );
        let areas = [area(0.0, 0.0, 1440.0, 900.0)];
        let spec = restore_or_default("business", &layout, &areas).unwrap();
        assert_eq!(spec.x, Some(480.0));
        assert_eq!(spec.y, Some(200.0));
        assert_eq!((spec.width, spec.height), (960.0, 680.0));
    }

    #[test]
    fn restore_keeps_the_maximized_flag_and_restore_size() {
        let mut layout = Layout::default();
        let mut geometry = Geometry::at(700.0, 500.0, Some(100.0), Some(120.0));
        geometry.maximized = true;
        layout.windows.insert("business".to_owned(), geometry);
        let areas = [area(0.0, 0.0, 1440.0, 900.0)];
        let spec = restore_or_default("business", &layout, &areas).unwrap();
        assert!(spec.maximized);
        assert_eq!((spec.width, spec.height), (700.0, 500.0));
        assert_eq!((spec.x, spec.y), (Some(100.0), Some(120.0)));
    }

    #[test]
    fn maximized_windows_report_their_restore_size() {
        let previous = Geometry::at(700.0, 500.0, Some(100.0), Some(120.0));
        let current = Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let kept = maximized_keeps_restore_size(Some(&previous), current, true);
        assert_eq!((kept.width, kept.height), (700.0, 500.0));
        assert_eq!((kept.x, kept.y), (100.0, 120.0));
        let floating = maximized_keeps_restore_size(Some(&previous), current, false);
        assert_eq!((floating.width, floating.height), (1920.0, 1080.0));
    }

    #[test]
    fn layout_round_trips_through_the_json_file() {
        let path = temp_path("layout.json");
        let mut layout = Layout::default();
        let mut geometry = Geometry::at(1000.0, 700.0, Some(10.0), Some(20.0));
        geometry.maximized = true;
        layout.windows.insert("business".to_owned(), geometry);
        write_layout(&path, &layout).unwrap();
        let restored = read_layout(&path);
        let spec = restore_or_default("business", &restored, &[]).unwrap();
        assert_eq!((spec.width, spec.height), (1000.0, 700.0));
        assert_eq!((spec.x, spec.y), (Some(10.0), Some(20.0)));
        assert!(spec.maximized);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn defaults_shrink_to_a_small_work_area() {
        let areas = [area(0.0, 0.0, 600.0, 500.0)];
        let spec = restore_or_default("business", &Layout::default(), &areas).unwrap();
        assert_eq!((spec.width, spec.height), (600.0, 500.0));
    }

    #[test]
    fn a_v3_layout_migrates_old_default_size_once_and_keeps_position() {
        let path = temp_path("legacy-layout.json");
        write_layout(
            &path,
            &Layout {
                version: 3,
                windows: BTreeMap::from([("business".to_owned(), {
                    let mut geometry = Geometry::at(1024.0, 640.0, Some(429.0), Some(102.0));
                    geometry.maximized = true;
                    geometry
                })]),
            },
        )
        .unwrap();
        let migrated = load_layout_file(&path);
        assert_eq!(migrated.version, LAYOUT_VERSION);
        let geometry = migrated.windows.get("business").unwrap();
        assert_eq!(
            (geometry.width, geometry.height),
            (DEFAULT_BUSINESS_WIDTH, DEFAULT_BUSINESS_HEIGHT)
        );
        assert_eq!((geometry.x, geometry.y), (Some(429.0), Some(102.0)));
        assert!(geometry.maximized);
        // The migration writes the version back, so a second load is a no-op.
        let reopened = read_layout(&path);
        assert_eq!(reopened.version, LAYOUT_VERSION);
        assert_eq!(reopened.windows.get("business").unwrap().width, 900.0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn all_legacy_business_defaults_reset_once() {
        for (width, height) in [
            (1024.0, 720.0),
            (1024.0, 640.0),
            (560.0, 420.0),
            (660.0, 600.0),
            (663.0, 458.0),
        ] {
            let path = temp_path(&format!("legacy-{width}x{height}.json"));
            write_layout(
                &path,
                &Layout {
                    version: 3,
                    windows: BTreeMap::from([(
                        "business".to_owned(),
                        Geometry::at(width, height, Some(10.0), Some(20.0)),
                    )]),
                },
            )
            .unwrap();
            let migrated = load_layout_file(&path);
            let geometry = migrated.windows.get("business").unwrap();
            assert_eq!(
                (geometry.width, geometry.height),
                (DEFAULT_BUSINESS_WIDTH, DEFAULT_BUSINESS_HEIGHT),
                "{width}x{height}"
            );
            assert_eq!((geometry.x, geometry.y), (Some(10.0), Some(20.0)));
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn a_v3_layout_keeps_a_user_custom_business_size() {
        let path = temp_path("custom-layout.json");
        write_layout(
            &path,
            &Layout {
                version: 3,
                windows: BTreeMap::from([(
                    "business".to_owned(),
                    Geometry::at(700.0, 500.0, Some(20.0), Some(30.0)),
                )]),
            },
        )
        .unwrap();
        let migrated = load_layout_file(&path);
        let geometry = migrated.windows.get("business").unwrap();
        assert_eq!((geometry.width, geometry.height), (700.0, 500.0));
        assert_eq!(migrated.version, LAYOUT_VERSION);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_current_layout_is_not_rewritten() {
        let path = temp_path("current-layout.json");
        let mut layout = Layout::default();
        layout.windows.insert(
            "business".to_owned(),
            Geometry::at(900.0, 633.0, Some(10.0), Some(20.0)),
        );
        write_layout(&path, &layout).unwrap();
        let loaded = load_layout_file(&path);
        assert_eq!(loaded.version, LAYOUT_VERSION);
        assert_eq!(
            (
                loaded.windows.get("business").unwrap().width,
                loaded.windows.get("business").unwrap().height
            ),
            (900.0, 633.0)
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_layout_returns_defaults() {
        let spec = restore_or_default("business", &Layout::default(), &[]).unwrap();
        assert_eq!((spec.width, spec.height), (900.0, 633.0));
        assert!(!spec.maximized);
        assert_eq!((spec.x, spec.y), (None, None));
    }
}
