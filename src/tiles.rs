//! 实时矢量瓦片流：OpenFreeMap（OpenMapTiles schema 的 MVT）按需加载。
//!
//! - Web Mercator XYZ 瓦片；zoom 按当前“米/像素”动态选择（视口约 2.5 瓦片宽）；
//! - 下载在任务线程完成，MVT 解码与网格构建复用桌面渲染管线；
//! - 瓦片 URL 模板启动时从 TileJSON 拉取（build 路径会滚动更新），失败回退内置模板；
//! - LRU 淘汰视口外瓦片并回收资产；磁盘缓存（桌面端）按 XYZ 键存储。

use std::collections::{HashMap, VecDeque};

use bevy::asset::Assets;
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::math::Vec2;
use bevy::prelude::*;
use bevy::tasks::futures::now_or_never;
use bevy::tasks::{IoTaskPool, Task};
use bevy::time::Time;
use bevy::window::{PrimaryWindow, Window};

use crate::camera::CameraRig;
use crate::geo::Projection;
use crate::globe::DataRing;
use crate::map_render::{build_map_layers, spawn_map_layers_at, MapLayer};
use crate::mvt::{decode_mvt, mvt_to_mapdata};
use crate::MapCtx;

const TILEJSON_URL: &str = "https://tiles.openfreemap.org/planet";
const FALLBACK_TEMPLATE: &str =
    "https://tiles.openfreemap.org/planet/20260906_080001_pt/{z}/{x}/{y}.pbf";
const TILE_MERCATOR_WIDTH: f64 = 40_075_016.7;

/// 最多同时下载的瓦片数
const MAX_INFLIGHT: usize = 4;
/// 两次请求之间的最小间隔（秒）
const REQUEST_INTERVAL: f32 = 0.15;
/// 缓存瓦片上限（超出按 LRU 淘汰视口外的）
const MAX_TILES: usize = 24;

// ---------- XYZ 瓦片坐标 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub z: u8,
    pub x: i32,
    pub y: i32,
}

/// 经纬度 → 指定 zoom 的瓦片坐标
pub fn xy_of(lat: f64, lon: f64, z: u8) -> (i32, i32) {
    let n = (1u64 << z) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor() as i32;
    let y = ((1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n).floor() as i32;
    (x, y)
}

/// 瓦片覆盖的经纬度外接框（南、西、北、东）
pub fn tile_bbox_latlon(k: TileKey) -> (f64, f64, f64, f64) {
    let n = (1u64 << k.z) as f64;
    let west = k.x as f64 / n * 360.0 - 180.0;
    let east = (k.x + 1) as f64 / n * 360.0 - 180.0;
    let north = (std::f64::consts::PI * (1.0 - 2.0 * k.y as f64 / n)).sinh().atan().to_degrees();
    let south =
        (std::f64::consts::PI * (1.0 - 2.0 * (k.y + 1) as f64 / n)).sinh().atan().to_degrees();
    (south, west, north, east)
}

/// 瓦片中心经纬度
pub fn tile_center_latlon(k: TileKey) -> (f64, f64) {
    let (s, w, n, e) = tile_bbox_latlon(k);
    ((s + n) * 0.5, (w + e) * 0.5)
}

/// 覆盖经纬度范围的全部瓦片键
pub fn keys_covering(lat_s: f64, lon_w: f64, lat_n: f64, lon_e: f64, z: u8) -> Vec<TileKey> {
    let (x0, y1) = xy_of(lat_s, lon_w, z);
    let (x1, y0) = xy_of(lat_n, lon_e, z);
    let mut keys = Vec::new();
    for x in x0.min(x1)..=x0.max(x1) {
        for y in y0.min(y1)..=y0.max(y1) {
            keys.push(TileKey { z, x, y });
        }
    }
    keys
}

/// 按视野米/像素选择瓦片 zoom：让视口约 2.5 个瓦片宽
pub fn zoom_for(mpp: f32, viewport_w_px: f32, lat_deg: f64) -> u8 {
    let target_tile_m = (mpp * viewport_w_px / 2.5).max(600.0) as f64;
    let z = (TILE_MERCATOR_WIDTH * lat_deg.to_radians().cos() / target_tile_m).log2().round();
    z.clamp(6.0, 14.0) as u8
}

// ---------- 平台 HTTP ----------

#[cfg(not(target_arch = "wasm32"))]
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let config = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build();
    let agent = config.new_agent();
    let resp = agent.get(url).call().map_err(|e| format!("{e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.into_body()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| format!("读取失败: {e}"))
}

#[cfg(target_arch = "wasm32")]
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or("无 window 对象")?;
    let resp_val = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|e| format!("网络失败: {e:?}"))?;
    let resp: web_sys::Response =
        resp_val.dyn_into().map_err(|_| "响应类型异常".to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let buf_promise = resp.array_buffer().map_err(|e| format!("{e:?}"))?;
    let buf = JsFuture::from(buf_promise).await.map_err(|e| format!("读取失败: {e:?}"))?;
    let array = js_sys::Uint8Array::new(&buf);
    Ok(array.to_vec())
}

// ---------- TileJSON 模板 ----------

/// 从 TileJSON 文本提取首个瓦片 URL 模板（避免引入 JSON 依赖）
pub fn extract_template(tilejson: &str) -> Option<String> {
    let idx = tilejson.find("\"tiles\"")?;
    let rest = &tilejson[idx..];
    let start = rest.find("https://")?;
    let rest = &rest[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

async fn fetch_template() -> Option<String> {
    let body = fetch_bytes(TILEJSON_URL).await.ok()?;
    let text = String::from_utf8(body).ok()?;
    extract_template(&text)
}

// ---------- 磁盘缓存（桌面端；web 端为空实现） ----------

#[cfg(not(target_arch = "wasm32"))]
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);
#[cfg(not(target_arch = "wasm32"))]
const CACHE_BUDGET: u64 = 1024 * 1024 * 1024;

#[cfg(not(target_arch = "wasm32"))]
fn cache_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache").join("ms-dos").join("tiles"))
        .unwrap_or_else(|| std::path::PathBuf::from("cache").join("tiles"))
}

#[cfg(not(target_arch = "wasm32"))]
fn tile_cache_path_in(dir: &std::path::Path, k: TileKey) -> std::path::PathBuf {
    dir.join(format!("tile_{}_{}_{}.pbf", k.z, k.x, k.y))
}

#[cfg(not(target_arch = "wasm32"))]
fn load_cached_in(dir: &std::path::Path, k: TileKey, ttl: std::time::Duration) -> Option<Vec<u8>> {
    let path = tile_cache_path_in(dir, k);
    let meta = std::fs::metadata(&path).ok()?;
    if meta.modified().ok()?.elapsed().ok()? > ttl {
        return None;
    }
    std::fs::read(&path).ok().filter(|s| !s.is_empty())
}

#[cfg(not(target_arch = "wasm32"))]
fn store_cached_in(dir: &std::path::Path, k: TileKey, data: &[u8]) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    enforce_cache_budget_in(dir, CACHE_BUDGET);
    let path = tile_cache_path_in(dir, k);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 目录超过预算时按 mtime 从旧到新删除
#[cfg(not(target_arch = "wasm32"))]
fn enforce_cache_budget_in(dir: &std::path::Path, budget: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime, u64)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((e.path(), meta.modified().ok()?, meta.len()))
        })
        .collect();
    let total: u64 = files.iter().map(|f| f.2).sum();
    if total <= budget {
        return;
    }
    files.sort_by_key(|f| f.1);
    let mut remaining = total;
    for (path, _, size) in files {
        if remaining <= budget / 2 {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            remaining = remaining.saturating_sub(size);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn load_cached(k: TileKey) -> Option<Vec<u8>> {
    load_cached_in(&cache_dir(), k, CACHE_TTL)
}

#[cfg(not(target_arch = "wasm32"))]
fn store_cached(k: TileKey, data: &[u8]) {
    store_cached_in(&cache_dir(), k, data)
}

#[cfg(target_arch = "wasm32")]
fn load_cached(_k: TileKey) -> Option<Vec<u8>> {
    None
}

#[cfg(target_arch = "wasm32")]
fn store_cached(_k: TileKey, _data: &[u8]) {}

// ---------- 瓦片载荷 ----------

pub struct TilePayload {
    pub layers: Vec<MapLayer>,
    pub origin: Vec2,
    pub n_polys: usize,
    pub n_lines: usize,
}

/// 同步工作体：下载 → MVT 解码 → 坐标换算 → 建网格（任务线程）
pub async fn fetch_tile(k: TileKey, template: &str) -> Result<TilePayload, String> {
    let url = template
        .replace("{z}", &k.z.to_string())
        .replace("{x}", &k.x.to_string())
        .replace("{y}", &k.y.to_string());
    let bytes = match load_cached(k) {
        Some(b) => b,
        None => {
            let raw = fetch_bytes(&url).await?;
            store_cached(k, &raw);
            raw
        }
    };
    // gzip 探测并解压（OpenFreeMap 默认未压缩，作保险）
    let bytes = if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        gunzip(&bytes)?
    } else {
        bytes
    };
    build_tile_payload(k, &bytes)
}

#[cfg(not(target_arch = "wasm32"))]
fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| format!("gzip 解压失败: {e}"))?;
    Ok(out)
}

#[cfg(target_arch = "wasm32")]
fn gunzip(_data: &[u8]) -> Result<Vec<u8>, String> {
    Err("web 端暂不支持 gzip 瓦片".into())
}

/// MVT → 瓦片局部坐标网格（纯函数，可测）
pub fn build_tile_payload(k: TileKey, mvt_bytes: &[u8]) -> Result<TilePayload, String> {
    let tile = decode_mvt(mvt_bytes);
    let global = Projection::global();
    let (s, w, n, e) = tile_bbox_latlon(k);
    let min = global.project(s, w);
    let max = global.project(n, e);
    let center = (min + max) * 0.5;
    // OpenMapTiles 各层 extent 均为 4096；y 翻转（MVT y 向下）
    let world = |p: (i32, i32)| -> Vec2 {
        let fx = p.0 as f64 / 4096.0;
        let fy = p.1 as f64 / 4096.0;
        let wx = min.x as f64 + fx * (max.x - min.x) as f64;
        let wy = max.y as f64 - fy * (max.y - min.y) as f64;
        Vec2::new((wx - center.x as f64) as f32, (wy - center.y as f64) as f32)
    };
    let mut map = mvt_to_mapdata(&tile, world);
    let n_polys = map.polys.len();
    let n_lines = map.lines.len();
    let half = (max - min) * 0.5;
    map.min = -half;
    map.max = half;
    let (clat, clon) = tile_center_latlon(k);
    let local_proj = Projection::new(clat, clon);
    let layers = build_map_layers(&map, &local_proj);
    Ok(TilePayload { layers, origin: center, n_polys, n_lines })
}

// ---------- 缓存与状态 ----------

enum TileStatus {
    Fetching(Task<Result<TilePayload, String>>),
    Loaded(Vec<crate::map_render::SpawnedMapLayer>),
    Failed { retry_at: f32 },
}

#[derive(Resource)]
pub struct LiveMap {
    pub enabled: bool,
}

/// 瓦片 URL 模板（启动时从 TileJSON 获取）
#[derive(Resource, Default)]
pub struct TileSource {
    pub template: Option<String>,
    fetching: Option<Task<Option<String>>>,
    tried: bool,
}

#[derive(Resource, Default)]
pub struct TileCache {
    tiles: HashMap<TileKey, TileStatus>,
    lru: VecDeque<TileKey>,
    /// 已加载区域（南、西、北、东），供地球数据环
    pub loaded_region: Option<(f64, f64, f64, f64)>,
    pub inflight: usize,
    pub failed: usize,
    last_request: f32,
}

impl TileCache {
    pub fn loaded_count(&self) -> usize {
        self.tiles.values().filter(|t| matches!(t, TileStatus::Loaded { .. })).count()
    }
}

// ---------- 主系统 ----------

#[allow(clippy::too_many_arguments)]
pub fn tile_stream_system(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    rig: Res<CameraRig>,
    ctx: Res<MapCtx>,
    window: Query<&Window, With<PrimaryWindow>>,
    time: Res<Time>,
    live: Res<LiveMap>,
    mut source: ResMut<TileSource>,
    mut cache: ResMut<TileCache>,
    mut ring: Option<ResMut<DataRing>>,
) {
    if !live.enabled {
        return;
    }

    // ---- 0. 瓦片 URL 模板 ----
    if source.template.is_none() {
        if let Some(task) = source.fetching.as_mut() {
            if let Some(result) = now_or_never(&mut *task) {
                source.fetching = None;
                if let Some(t) = result {
                    eprintln!("[tiles] TileJSON 模板: {t}");
                    source.template = Some(t);
                }
            }
        } else if !source.tried {
            source.tried = true;
            let task = IoTaskPool::get().spawn(async { fetch_template().await });
            source.fetching = Some(task);
            return;
        } else {
            // 获取失败：回退内置模板
            eprintln!("[tiles] TileJSON 获取失败，使用内置模板");
            source.template = Some(FALLBACK_TEMPLATE.to_string());
        }
    }
    let Some(template) = source.template.clone() else { return };

    // ---- 1. 视口 → 需要的瓦片 ----
    let (w_px, h_px) = window.single().map(|w| (w.width(), w.height())).unwrap_or((1600.0, 900.0));
    let half = Vec2::new(w_px * rig.mpp * 0.5, h_px * rig.mpp * 0.5);
    let (lat_s, lon_w) = ctx.proj.unproject(rig.target - half);
    let (lat_n, lon_e) = ctx.proj.unproject(rig.target + half);
    let (center_lat, _) = ctx.proj.unproject(rig.target);
    let z = zoom_for(rig.mpp, w_px, center_lat);
    // 外扩半瓦片，避免边缘露出底色
    let tile_deg = 360.0 / (1u64 << z) as f64;
    let wanted = keys_covering(
        lat_s - tile_deg,
        lon_w - tile_deg,
        lat_n + tile_deg,
        lon_e + tile_deg,
        z,
    );
    for k in &wanted {
        if let Some(q) = cache.lru.iter().position(|x| x == k) {
            cache.lru.remove(q);
            cache.lru.push_back(*k);
        }
    }

    // ---- 2. 收割完成的任务 ----
    let mut finished: Vec<(TileKey, Result<TilePayload, String>)> = Vec::new();
    for (k, st) in cache.tiles.iter_mut() {
        if let TileStatus::Fetching(task) = st {
            if let Some(result) = now_or_never(&mut *task) {
                finished.push((*k, result));
            }
        }
    }
    for (k, result) in finished {
        cache.inflight = cache.inflight.saturating_sub(1);
        match result {
            Ok(payload) => {
                let spawned = spawn_map_layers_at(
                    &mut commands,
                    &mut meshes,
                    &mut materials,
                    payload.layers,
                    payload.origin,
                );
                eprintln!(
                    "[tile {}/{}/{}] 加载完成: {} 面 / {} 线",
                    k.z, k.x, k.y, payload.n_polys, payload.n_lines
                );
                cache.tiles.insert(k, TileStatus::Loaded(spawned));
                cache.lru.push_back(k);
                let (s, w, n, e) = tile_bbox_latlon(k);
                cache.loaded_region = Some(match cache.loaded_region {
                    Some((s0, w0, n0, e0)) => (s0.min(s), w0.min(w), n0.max(n), e0.max(e)),
                    None => (s, w, n, e),
                });
                if let Some(ring) = ring.as_deref_mut() {
                    ring.bbox = cache.loaded_region;
                }
            }
            Err(err) => {
                eprintln!("[tile {}/{}/{}] 失败（20s 后重试）: {err}", k.z, k.x, k.y);
                cache.failed += 1;
                cache.tiles.insert(
                    k,
                    TileStatus::Failed { retry_at: time.elapsed_secs() + 20.0 },
                );
            }
        }
    }

    // ---- 3. 发起新请求（节流 + 并发上限）----
    cache.last_request += time.delta().as_secs_f32();
    if cache.last_request >= REQUEST_INTERVAL && cache.inflight < MAX_INFLIGHT {
        let now = time.elapsed_secs();
        let candidate = wanted
            .iter()
            .find(|k| match cache.tiles.get(k) {
                None => true,
                Some(TileStatus::Failed { retry_at }) => now >= *retry_at,
                _ => false,
            })
            .copied();
        if let Some(k) = candidate {
            cache.last_request = 0.0;
            cache.inflight += 1;
            cache.failed = cache.failed.saturating_sub(1);
            let t = template.clone();
            let task: Task<Result<TilePayload, String>> =
                IoTaskPool::get().spawn(async move { fetch_tile(k, &t).await });
            cache.tiles.insert(k, TileStatus::Fetching(task));
        }
    }

    // ---- 4. LRU 淘汰视口外的旧瓦片 ----
    while cache.lru.len() > MAX_TILES {
        let Some(victim) = cache.lru.pop_front() else { break };
        if wanted.contains(&victim) {
            cache.lru.push_front(victim);
            break;
        }
        if let Some(TileStatus::Loaded(spawned)) = cache.tiles.remove(&victim) {
            for layer in spawned {
                commands.entity(layer.entity).despawn();
                let (mesh, mat) = (layer.mesh, layer.material);
                commands.queue(move |world: &mut bevy::ecs::world::World| {
                    world.resource_mut::<Assets<Mesh>>().remove(mesh.id());
                    world.resource_mut::<Assets<ColorMaterial>>().remove(mat.id());
                });
            }
            eprintln!("[tile {}/{}/{}] LRU 卸载", victim.z, victim.x, victim.y);
        }
    }
}

/// 全球大地背景（数据区外仍是陆地板）
pub fn spawn_global_background(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
) {
    let quad = Mesh::from(bevy::math::primitives::Rectangle::new(42_000_000.0, 42_000_000.0));
    let mesh = meshes.add(quad);
    let mat = materials.add(ColorMaterial::from(crate::map_render::palette::LAND_BG));
    commands.spawn((Mesh2d(mesh), MeshMaterial2d(mat), Transform::default()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_xy_honolulu() {
        // 檀香山 z13（与线上真实瓦片一致的坐标）
        let (x, y) = xy_of(21.31, -157.86, 13);
        assert_eq!((x, y), (503, 3599));
    }

    #[test]
    fn bbox_roundtrip() {
        let k = TileKey { z: 13, x: 503, y: 3599 };
        let (s, w, n, e) = tile_bbox_latlon(k);
        assert!(s < 21.31 && 21.31 < n, "lat 不在瓦片内: {s}..{n}");
        assert!(w < -157.86 && -157.86 < e, "lon 不在瓦片内: {w}..{e}");
        let (clat, clon) = tile_center_latlon(k);
        let (x2, y2) = xy_of(clat, clon, 13);
        assert_eq!((x2, y2), (k.x, k.y));
    }

    #[test]
    fn covering_keys() {
        let keys = keys_covering(21.30, -158.05, 21.45, -157.85, 13);
        assert!(keys.contains(&TileKey { z: 13, x: 503, y: 3599 }));
        assert!(keys.len() >= 2);
    }

    #[test]
    fn zoom_selection() {
        let far = zoom_for(1200.0, 1600.0, 21.35);
        let near = zoom_for(5.0, 1600.0, 21.35);
        assert!(far <= 10, "far zoom = {far}");
        assert_eq!(near, 14);
        let mid = zoom_for(60.0, 1600.0, 21.35);
        assert!(far <= mid && mid <= near);
    }

    #[test]
    fn template_extraction() {
        let json = r#"{"tilejson":"3.0.0","tiles":["https://tiles.openfreemap.org/planet/20260906_080001_pt/{z}/{x}/{y}.pbf"]}"#;
        let t = extract_template(json).unwrap();
        assert!(t.starts_with("https://") && t.contains("{z}/{x}/{y}"));
        assert!(extract_template("no url here").is_none());
    }
}
