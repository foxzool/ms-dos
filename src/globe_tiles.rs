//! 地球贴图瓦片流：NASA GIBS Blue Marble 着色地形（Web Mercator XYZ，z0-8）。
//!
//! 单张 2048 贴图在贴近时放大 ~17 倍导致海洋区域糊成灰白；
//! 此模块按 Cesium imagery-quadtree 思路做球面分区网格 + 分级贴图流：
//! - 目标级别由视距决定（与地图侧 zoom_for 同族公式）；
//! - 只加载朝向相机的半球瓦片（法线 · 视线剔除）；
//! - 异步下载 + 桌面磁盘缓存 + LRU（复用瓦片流架构）；
//! - 远景（全球）回落到内嵌底图球，瓦片层隐藏。

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::asset::Assets;
use bevy::ecs::entity::Entity;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::prelude::*;
use bevy::render::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::asset::RenderAssetUsages;
use bevy::tasks::{futures::now_or_never, IoTaskPool, Task};

use crate::globe::{lat_lon_to_vec3, GlobeCamera, GlobeRig, GLOBE_RADIUS};
#[cfg(not(target_arch = "wasm32"))]
use crate::tiles::{load_cached_tile_bytes, store_cached_tile_bytes};

const GIBS_TEMPLATE: &str = "https://gibs.earthdata.nasa.gov/wmts/epsg3857/best/BlueMarble_ShadedRelief_Bathymetry/default/GoogleMapsCompatible_Level8";
/// 近观下限：z8 ≈ 611 m/px；更近无更高源，直接放大
const GIBS_MAX_Z: u8 = 8;
const MAX_TILES: usize = 120; // 全球 z4 半球约 100 张；z8 视场内远少于上限
// 请求强度刻意保守：GIBS/OpenFreeMap 均按 IP 限流（实测 403），高并发得不偿失
const MAX_INFLIGHT: usize = 3;
const REQUEST_INTERVAL: f32 = 0.3;

// ---------- 球面分区网格 ----------

/// Web Mercator XYZ 瓦片的球面 patch 网格（经纬细分，法线朝外）。
/// 顶点纬度按 Mercator y 均匀插值——与贴图像素行严格对齐；
/// 若按线性纬度插值，y=0 行（极区）瓦片纹理会被拉伸错位（冰盖形状变形）。
pub fn globe_patch_mesh(z: u8, x: u32, y: u32, radius: f32, seg_x: usize, seg_y: usize) -> Mesh {
    let (lat_s, lon_w, lat_n, lon_e) = tile_bbox(z, x, y);
    let merc_n = lat_n.to_radians().tan().asinh();
    let merc_s = lat_s.to_radians().tan().asinh();
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    for j in 0..=seg_y {
        let v = j as f64 / seg_y as f64;
        let lat = (merc_s + (merc_n - merc_s) * v).sinh().atan().to_degrees();
        for i in 0..=seg_x {
            let u = i as f64 / seg_x as f64;
            let lon = lon_w + (lon_e - lon_w) * u;
            let p = lat_lon_to_vec3(lat as f32, lon as f32, radius);
            positions.push(p.to_array());
            normals.push(p.normalize().to_array());
            uvs.push([u as f32, 1.0 - v as f32]); // 贴图 v 向下
        }
    }
    let w = seg_x + 1;
    let mut indices: Vec<u32> = Vec::new();
    for j in 0..seg_y {
        for i in 0..seg_x {
            let a = (j * w + i) as u32;
            indices.extend_from_slice(&[a, a + w as u32, a + 1, a + 1, a + w as u32, a + w as u32 + 1]);
        }
    }
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

/// Web Mercator 瓦片 bbox（南、西、北、东，度）
pub fn tile_bbox(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let n = (1u64 << z) as f64;
    let west = x as f64 / n * 360.0 - 180.0;
    let east = (x + 1) as f64 / n * 360.0 - 180.0;
    let north = (std::f64::consts::PI - y as f64 / n * std::f64::consts::TAU)
        .sinh()
        .atan()
        .to_degrees();
    let south = (std::f64::consts::PI - (y + 1) as f64 / n * std::f64::consts::TAU)
        .sinh()
        .atan()
        .to_degrees();
    (south, west, north, east)
}

// ---------- 下载（复用平台 HTTP 与磁盘缓存） ----------

#[cfg(not(target_arch = "wasm32"))]
async fn fetch_gibs(url: &str) -> Result<Vec<u8>, String> {
    let config = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build();
    let agent = config.new_agent();
    let resp = agent.get(url).call().map_err(|e| format!("{e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.into_body()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| format!("读取失败: {e}"))
}

pub struct GlobeTilePayload {
    pub image: bevy::image::Image,
}

pub async fn fetch_globe_tile(z: u8, x: u32, y: u32) -> Result<GlobeTilePayload, String> {
    let url = format!("{GIBS_TEMPLATE}/{z}/{y}/{x}.jpg");
    #[cfg(target_arch = "wasm32")]
    let bytes = {
        let key = crate::web_cache::gibs_cache_key(z, x, y);
        crate::web_cache::cached_fetch(&url, &key).await?
    };
    #[cfg(not(target_arch = "wasm32"))]
    let bytes = match load_cached_tile_bytes(z, x, y) {
        Some(b) => b,
        None => {
            let raw = fetch_gibs(&url).await?;
            store_cached_tile_bytes(z, x, y, &raw);
            raw
        }
    };
    // JPEG 解码
    let cursor = std::io::Cursor::new(&bytes);
    let img = image::ImageReader::with_format(cursor, image::ImageFormat::Jpeg)
        .decode()
        .map_err(|e| format!("JPEG 解码失败: {e}"))?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    let texture = bevy::image::Image::new(
        bevy::render::render_resource::Extent3d { width: w, height: h, ..default() },
        bevy::render::render_resource::TextureDimension::D2,
        img.into_raw(),
        bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    Ok(GlobeTilePayload { image: texture })
}

// ---------- 缓存与系统 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlobeKey {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

#[derive(Component)]
pub(crate) struct GlobeTileMesh {
    key: GlobeKey,
}

enum GlobeStatus {
    Fetching(Task<Result<GlobeTilePayload, String>>),
    Loaded { entity: Entity },
    Failed { retry_at: f32 },
}

#[derive(Resource, Default)]
pub struct GlobeTileCache {
    tiles: HashMap<GlobeKey, GlobeStatus>,
    lru: VecDeque<GlobeKey>,
    pub inflight: usize,
    last_request: f32,
}

impl GlobeTileCache {
    pub fn loaded(&self) -> usize {
        self.tiles.values().filter(|t| matches!(t, GlobeStatus::Loaded { .. })).count()
    }

    pub fn capacity(&self) -> usize {
        MAX_TILES
    }
}

/// 视距 → 目标瓦片级（z8 ≈ 611 m/px；全球 ~1 亿米/px → z0）
pub fn globe_zoom_for(distance: f32) -> u8 {
    // 期望地面分辨率 ≈ 视高地表米数/视口像素，地表分辨率按视距线性近似
    let alt = (distance - GLOBE_RADIUS).max(1000.0);
    // 瓦片级地面分辨率（每 256px 瓦片）：40075017/256/2^z，屏幕需求 alt/900
    let need = alt as f64 / 900.0; // m/px
    let z = ((40_075_016.7 / 256.0) / need).log2().ceil();
    z.clamp(0.0, GIBS_MAX_Z as f64) as u8
}

/// 仍处于 Failed/Fetching 的瓦片的所有祖先键——它们是这些未就绪区域的兜底显示，
/// 不得被 LRU 淘汰（否则失败区域露出内嵌底图形成空洞）。
fn fallback_protected(tiles: &HashMap<GlobeKey, GlobeStatus>) -> HashSet<GlobeKey> {
    let mut protected = HashSet::new();
    for (k, st) in tiles {
        if !matches!(st, GlobeStatus::Failed { .. } | GlobeStatus::Fetching(_)) {
            continue;
        }
        let mut z = k.z;
        while z > 0 {
            z -= 1;
            let d = k.z - z;
            protected.insert(GlobeKey { z, x: k.x >> d, y: k.y >> d });
        }
    }
    protected
}

/// 粗瓦片是否被目标级别下它覆盖的子网格“完全加载”——完全覆盖才隐藏，
/// 避免新旧级别同半径 z-fighting 闪替，同时保证未加载完成前旧瓦片兜底显示。
fn fully_covered_by_target(key: GlobeKey, z_target: u8, tiles: &HashMap<GlobeKey, GlobeStatus>) -> bool {
    if key.z >= z_target {
        return false;
    }
    let n = 1u32 << (z_target - key.z);
    for ty in 0..n {
        for tx in 0..n {
            let k = GlobeKey { z: z_target, x: key.x * n + tx, y: key.y * n + ty };
            if !matches!(tiles.get(&k), Some(GlobeStatus::Loaded { .. })) {
                return false;
            }
        }
    }
    true
}

/// 主系统：按 GlobeRig 加载/卸载/收割球面贴图瓦片
pub fn globe_tile_system(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<bevy::image::Image>>,
    mut materials: ResMut<Assets<bevy::pbr::StandardMaterial>>,
    rig: Res<GlobeRig>,
    cams: Query<&Transform, With<GlobeCamera>>,
    time: Res<Time>,
    mut cache: ResMut<GlobeTileCache>,
    mut q_tiles: Query<(&GlobeTileMesh, &mut Visibility)>,
) {
    let Ok(cam_t) = cams.single() else { return };
    let z = globe_zoom_for(rig.distance);
    let n = 1u32 << z;
    let cam_pos = cam_t.translation;

    // ---- 收割 ----
    let mut finished: Vec<(GlobeKey, Result<GlobeTilePayload, String>)> = Vec::new();
    for (k, st) in cache.tiles.iter_mut() {
        if let GlobeStatus::Fetching(task) = st {
            if let Some(result) = now_or_never(&mut *task) {
                finished.push((*k, result));
            }
        }
    }
    for (k, result) in finished {
        cache.inflight = cache.inflight.saturating_sub(1);
        match result {
            Ok(payload) => {
                let mesh = meshes.add(globe_patch_mesh(k.z, k.x, k.y, GLOBE_RADIUS * 1.015, 16, 8));
                let tex = images.add(payload.image);
                // 暗蓝色调：Blue Marble Bathymetry 的海洋/冰盖本色偏亮（测深渐变+南极冰），
                // 乘以冷色 tint 压回战术地球观感
                let mat = materials.add(bevy::pbr::StandardMaterial {
                    base_color_texture: Some(tex),
                    base_color: bevy::color::Color::srgb(0.52, 0.60, 0.72),
                    unlit: true,
                    cull_mode: None,
                    ..default()
                });
                let entity = commands
                    .spawn((
                        Mesh3d(mesh),
                        bevy::pbr::MeshMaterial3d(mat),
                        Transform::default(),
                        Visibility::Hidden, // 可见性由下方“祖先隐藏”逻辑逐帧决定
                        GlobeTileMesh { key: k },
                    ))
                    .id();
                cache.tiles.insert(k, GlobeStatus::Loaded { entity });
                cache.lru.push_back(k);
            }
            Err(_err) => {
                cache.tiles.insert(k, GlobeStatus::Failed { retry_at: time.elapsed_secs() + 30.0 });
            }
        }
    }

    // ---- 可见瓦片（朝向相机 + z 网格） ----
    let mut wanted: Vec<GlobeKey> = Vec::new();
    let cam_dir = cam_pos.normalize();
    for ty in 0..n {
        for tx in 0..n {
            let (lat_s, lon_w, lat_n, lon_e) = tile_bbox(z, tx, ty);
            let center = lat_lon_to_vec3(((lat_s + lat_n) / 2.0) as f32, ((lon_w + lon_e) / 2.0) as f32, 1.0);
            // 半球剔除：分区中心法线与相机方向同侧（留 15° 余量）
            if center.dot(cam_dir) < -0.25 {
                continue;
            }
            wanted.push(GlobeKey { z, x: tx, y: ty });
        }
    }
    for k in &wanted {
        if let Some(q) = cache.lru.iter().position(|x| x == k) {
            cache.lru.remove(q);
            cache.lru.push_back(*k);
        }
    }

    // ---- 发起（节流） ----
    cache.last_request += time.delta().as_secs_f32();
    if cache.last_request >= REQUEST_INTERVAL && cache.inflight < MAX_INFLIGHT {
        let now = time.elapsed_secs();
        if let Some(k) = wanted.iter().copied().find(|k| match cache.tiles.get(k) {
            None => true,
            Some(GlobeStatus::Failed { retry_at }) => now >= *retry_at,
            _ => false,
        }) {
            cache.last_request = 0.0;
            cache.inflight += 1;
            let task = IoTaskPool::get().spawn(async move { fetch_globe_tile(k.z, k.x, k.y).await });
            cache.tiles.insert(k, GlobeStatus::Fetching(task));
        }
    }

    // ---- 可见性：被更细级别覆盖的祖先瓦片隐藏（消除新旧级别同半径闪替） ----
    for (m, mut vis) in &mut q_tiles {
        *vis = if fully_covered_by_target(m.key, z, &cache.tiles) {
            Visibility::Hidden
        } else {
            Visibility::Visible
        };
    }

    // ---- LRU 卸载（wanted 与“失败/加载中区域的兜底祖先”都不可踢） ----
    let protected = fallback_protected(&cache.tiles);
    let mut attempts = cache.lru.len();
    while cache.lru.len() > MAX_TILES && attempts > 0 {
        attempts -= 1;
        let Some(victim) = cache.lru.pop_front() else { break };
        if wanted.contains(&victim) || protected.contains(&victim) {
            cache.lru.push_back(victim);
            continue;
        }
        if let Some(GlobeStatus::Loaded { entity }) = cache.tiles.remove(&victim) {
            commands.entity(entity).despawn();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::render::mesh::VertexAttributeValues;

    #[test]
    fn coarse_tile_hidden_only_when_fully_covered() {
        let mut tiles = HashMap::new();
        let ent = Entity::PLACEHOLDER;
        // z4(2,1) 是 z5(4,2)/(4,3)/(5,2)/(5,3) 四个子块的祖先
        let coarse = GlobeKey { z: 4, x: 2, y: 1 };
        // 全部未加载：不隐藏
        assert!(!fully_covered_by_target(coarse, 5, &tiles));
        // 三个子块加载：仍不隐藏（需完全覆盖）
        for (x, y) in [(4, 2), (5, 2), (4, 3)] {
            tiles.insert(GlobeKey { z: 5, x, y }, GlobeStatus::Loaded { entity: ent });
        }
        assert!(!fully_covered_by_target(coarse, 5, &tiles));
        // 第四个子块加载后：隐藏
        tiles.insert(GlobeKey { z: 5, x: 5, y: 3 }, GlobeStatus::Loaded { entity: ent });
        assert!(fully_covered_by_target(coarse, 5, &tiles));
        // 目标级别不高于自身：不隐藏（更高精细缓存直接显示）
        assert!(!fully_covered_by_target(GlobeKey { z: 5, x: 4, y: 2 }, 5, &tiles));
        assert!(!fully_covered_by_target(coarse, 4, &tiles));
    }

    #[test]
    fn failed_tiles_protect_their_ancestors_from_eviction() {
        let mut tiles = HashMap::new();
        let ent = Entity::PLACEHOLDER;
        // z5(4,2) 失败 → 其祖先 z4(2,1)/z3(1,0)/…/z0 全部受保护
        tiles.insert(GlobeKey { z: 5, x: 4, y: 2 }, GlobeStatus::Failed { retry_at: 1.0 });
        let protected = fallback_protected(&tiles);
        assert!(protected.contains(&GlobeKey { z: 4, x: 2, y: 1 }));
        assert!(protected.contains(&GlobeKey { z: 3, x: 1, y: 0 }));
        assert!(protected.contains(&GlobeKey { z: 0, x: 0, y: 0 }));
        // 兄弟分支不受保护
        assert!(!protected.contains(&GlobeKey { z: 4, x: 3, y: 1 }));
        // Loaded/实体键不产生保护
        let mut clean = HashMap::new();
        clean.insert(GlobeKey { z: 5, x: 4, y: 2 }, GlobeStatus::Loaded { entity: ent });
        assert!(fallback_protected(&clean).is_empty());
    }

    /// 顶点纬度必须按 Web Mercator 插值（与贴图行对齐），而非线性纬度：
    /// 线性中点会显著低于 Mercator 中点（极区瓦片尤甚）。
    #[test]
    fn patch_vertices_follow_mercator() {
        let m = globe_patch_mesh(5, 0, 0, 1.0, 4, 8);
        let pos = m.attribute(Mesh::ATTRIBUTE_POSITION).expect("应有位置属性");
        let VertexAttributeValues::Float32x3(vals) = pos else { panic!("位置应为 Float32x3") };
        let to_lat = |j: usize| (vals[j * 5][1] as f32).asin().to_degrees();
        let (_lat_s, _lon_w, lat_n, _lon_e) = tile_bbox(5, 0, 0);
        // 中点纬度高于线性中点（Mercator 在北侧聚集）
        let south = (std::f64::consts::PI - 1.0f64 / 32.0 * std::f64::consts::TAU)
            .sinh().atan().to_degrees();
        let linear_mid = (lat_n + south) / 2.0;
        assert!(to_lat(4) > linear_mid as f32, "中点纬度 {} 应高于线性中点 {}", to_lat(4), linear_mid);
        assert!(to_lat(8) <= lat_n as f32 + 1e-3, "北边行不应越过瓦片北界");
    }
}
