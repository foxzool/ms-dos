//! web 端视图与 URL hash 同步（deep-linking）。
//!
//! - 格式：`#map=纬度,经度,米每像素`（地图态）/ `#globe=纬度,经度`（地球态）
//! - 运行时用 `history.replaceState` 更新（不产生历史条目），带节流与变化阈值；
//! - 启动时解析 hash 恢复初始视图；
//! - 解析/格式化为纯函数，双端可单测；浏览器 API 调用仅 wasm 编译。

use bevy::prelude::*;
use bevy::ecs::system::{Local, Res};

use crate::camera::CameraRig;
use crate::globe::{AppState, GlobeRig};
use crate::MapCtx;

/// URL 恢复的初始视图
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InitialView {
    Map { lat: f64, lon: f64, mpp: f32 },
    Globe { lat: f32, lon: f32 },
}

/// 解析 hash（如 `#map=21.355,-157.925,17.8`）
pub fn parse_hash(s: &str) -> Option<InitialView> {
    let body = s.trim_start_matches('#');
    let (kind, rest) = body.split_once('=')?;
    match kind {
        "map" => {
            let mut it = rest.split(',');
            let lat: f64 = it.next()?.parse().ok()?;
            let lon: f64 = it.next()?.parse().ok()?;
            let mpp: f32 = it.next()?.parse().ok()?;
            if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || !(1.2..=1200.0).contains(&mpp) {
                return None;
            }
            Some(InitialView::Map { lat, lon, mpp })
        }
        "globe" => {
            let mut it = rest.split(',');
            let lat: f32 = it.next()?.parse().ok()?;
            let lon: f32 = it.next()?.parse().ok()?;
            if !(-85.0..=85.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                return None;
            }
            Some(InitialView::Globe { lat, lon })
        }
        _ => None,
    }
}

/// 地图态 hash（纬度 5 位 ≈ 1m，mpp 2 位）
pub fn format_map(lat: f64, lon: f64, mpp: f32) -> String {
    format!("#map={lat:.5},{lon:.5},{mpp:.2}")
}

/// 地球态 hash
pub fn format_globe(lat: f32, lon: f32) -> String {
    format!("#globe={lat:.3},{lon:.3}")
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
    time: Res<Time>,
    mut last: Local<LastUrl>,
) {
    last.since += time.delta().as_secs_f32();
    if last.since < 0.3 {
        return;
    }
    let text = match *state.get() {
        AppState::Map => {
            let (lat, lon) = ctx.proj.unproject(map_rig.target);
            format_map(lat, lon, map_rig.mpp)
        }
        AppState::Globe => format_globe(globe_rig.lat, globe_rig.lon),
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
        let url = format_map(21.35498, -157.92512, 17.83);
        match parse_hash(&url) {
            Some(InitialView::Map { lat, lon, mpp }) => {
                assert!((lat - 21.35498).abs() < 1e-5);
                assert!((lon - (-157.92512)).abs() < 1e-5);
                assert!((mpp - 17.83).abs() < 0.02);
            }
            other => panic!("解析失败: {other:?}"),
        }
    }

    #[test]
    fn globe_roundtrip() {
        let url = format_globe(35.68, 139.77);
        match parse_hash(&url) {
            Some(InitialView::Globe { lat, lon }) => {
                assert!((lat - 35.68).abs() < 1e-3 && (lon - 139.77).abs() < 1e-3);
            }
            other => panic!("解析失败: {other:?}"),
        }
    }

    #[test]
    fn invalid_hashes_rejected() {
        assert!(parse_hash("").is_none());
        assert!(parse_hash("#map=1,2").is_none()); // 缺 mpp
        assert!(parse_hash("#map=99,0,10").is_none()); // 纬度越界
        assert!(parse_hash("#map=0,0,99999").is_none()); // mpp 越界
        assert!(parse_hash("#foo=1,2,3").is_none());
    }
}
