//! web 端视图与 URL hash 同步（deep-linking）。
//!
//! - 格式：`#map=纬度,经度,米每像素`（地图态）/ `#globe=纬度,经度`（地球态）
//! - 运行时用 `history.replaceState` 更新（不产生历史条目），带节流与变化阈值；
//! - 启动时解析 hash 恢复初始视图；
//! - 解析/格式化为纯函数，双端可单测；浏览器 API 调用仅 wasm 编译。

use bevy::prelude::*;
#[cfg(target_arch = "wasm32")]
use bevy::ecs::system::{Local, Res};

#[cfg(target_arch = "wasm32")]
use crate::camera::CameraRig;
#[cfg(target_arch = "wasm32")]
use crate::globe::{AppState, GlobeRig};
#[cfg(target_arch = "wasm32")]
use crate::MapCtx;

/// URL 恢复的初始视图。zoom 以“视高”（米）表达：
/// - Map：视口地面垂直跨度（= mpp × 窗口高度像素），跨设备一致
/// - Globe：相机离地表高度（= 距球心距离 − 地球半径）
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // native 下仅单元测试使用
pub enum InitialView {
    Map { lat: f64, lon: f64, alt_m: f32 },
    Globe { lat: f32, lon: f32, alt_m: f32 },
}

/// 解析 hash（如 `#map=21.355,-157.925,17.8`）
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn parse_hash(s: &str) -> Option<InitialView> {
    let body = s.trim_start_matches('#');
    let (kind, rest) = body.split_once('=')?;
    match kind {
        "map" => {
            let mut it = rest.split(',');
            let lat: f64 = it.next()?.parse().ok()?;
            let lon: f64 = it.next()?.parse().ok()?;
            let alt: f32 = it.next()?.parse().ok()?;
            // 视高合法范围：约 100m（近观）… 2,000km（全球缩放）
            if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || !(100.0..=2_000_000.0).contains(&alt) {
                return None;
            }
            Some(InitialView::Map { lat, lon, alt_m: alt })
        }
        "globe" => {
            let mut it = rest.split(',');
            let lat: f32 = it.next()?.parse().ok()?;
            let lon: f32 = it.next()?.parse().ok()?;
            // 高度可选（缺省 = 全球视角 2.9R ≈ 12,000km 离地）
            let alt: f32 = match it.next() {
                Some(v) => v.parse().ok()?,
                None => 12_000_000.0,
            };
            if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || !(10_000.0..=20_000_000.0).contains(&alt) {
                return None;
            }
            Some(InitialView::Globe { lat, lon, alt_m: alt })
        }
        _ => None,
    }
}

/// 地图态 hash（纬度 5 位 ≈ 1m；zoom = 视高米）
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn format_map(lat: f64, lon: f64, alt_m: f32) -> String {
    format!("#map={lat:.5},{lon:.5},{alt_m:.0}")
}

/// 地球态 hash（zoom = 离地表高度米，缺省全球）
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn format_globe(lat: f32, lon: f32, alt_m: f32) -> String {
    format!("#globe={lat:.3},{lon:.3},{alt_m:.0}")
}

#[cfg(target_arch = "wasm32")]
pub fn raw_hash() -> String {
    web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
pub fn read_initial_view() -> Option<InitialView> {
    parse_hash(&raw_hash())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_initial_view() -> Option<InitialView> {
    None
}

#[derive(Default)]
#[cfg(target_arch = "wasm32")]
pub(crate) struct LastUrl {
    text: String,
    since: f32,
}

/// 视图 → URL：replaceState 更新 hash（节流 300ms + 变化阈值，避免历史膨胀与高频调用）
#[cfg(target_arch = "wasm32")]
pub fn sync_url_system(
    state: Res<State<AppState>>,
    map_rig: Res<CameraRig>,
    globe_rig: Res<GlobeRig>,
    ctx: Res<MapCtx>,
    window: Query<&bevy::window::Window, bevy::ecs::query::With<bevy::window::PrimaryWindow>>,
    time: Res<Time>,
    mut last: Local<LastUrl>,
) {
    last.since += time.delta().as_secs_f32();
    if last.since < 0.3 {
        return;
    }
    let win_h = window.single().map(|w| w.height()).unwrap_or(900.0);
    let text = match *state.get() {
        AppState::Map => {
            let (lat, lon) = ctx.proj.unproject(map_rig.target);
            // 视高 = 视口地面垂直跨度
            format_map(lat, lon, map_rig.mpp * win_h)
        }
        AppState::Globe => format_globe(
            globe_rig.lat,
            globe_rig.lon,
            // 离地表高度（视距公式的逆：alt = (dist − R)）
            (globe_rig.distance - crate::globe::GLOBE_RADIUS).max(0.0),
        ),
    };
    if text == last.text {
        return;
    }
    if let Some(window) = web_sys::window() {
        let history = window.history().ok();
        if let Some(h) = history {
            // replaceState：更新地址不压入浏览器历史
            let _ = h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&text));
            last.text = text;
            last.since = 0.0;
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn sync_url_system() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_roundtrip() {
        let url = format_map(21.35498, -157.92512, 10_008.0);
        match parse_hash(&url) {
            Some(InitialView::Map { lat, lon, alt_m }) => {
                assert!((lat - 21.35498).abs() < 1e-5);
                assert!((lon - (-157.92512)).abs() < 1e-5);
                assert!((alt_m - 10_008.0).abs() < 1.0);
            }
            other => panic!("解析失败: {other:?}"),
        }
    }

    #[test]
    fn globe_roundtrip() {
        let url = format_globe(35.68, 139.77, 12_000_000.0);
        match parse_hash(&url) {
            Some(InitialView::Globe { lat, lon, alt_m }) => {
                assert!((lat - 35.68).abs() < 1e-3 && (lon - 139.77).abs() < 1e-3);
                assert!((alt_m - 12_000_000.0).abs() < 1.0);
            }
            other => panic!("解析失败: {other:?}"),
        }
    }

    #[test]
    fn globe_default_alt() {
        // 高度缺省 → 全球视角
        match parse_hash("#globe=21.4,-158.0") {
            Some(InitialView::Globe { alt_m, .. }) => assert!((alt_m - 12_000_000.0).abs() < 1.0),
            other => panic!("解析失败: {other:?}"),
        }
    }

    #[test]
    fn invalid_hashes_rejected() {
        assert!(parse_hash("").is_none());
        assert!(parse_hash("#map=1,2").is_none()); // 缺高度
        assert!(parse_hash("#map=99,0,1000").is_none()); // 纬度越界
        assert!(parse_hash("#map=0,0,99").is_none()); // 高度低于下限
        assert!(parse_hash("#map=0,0,9999999").is_none()); // 高度超上限
        assert!(parse_hash("#foo=1,2,3").is_none());
    }
}
