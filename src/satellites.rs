//! 实时卫星位置渲染：CelesTrak TLE + SGP4 轨道传播。
//!
//! - 启动（Globe 态）拉取 stations + visual 两组 TLE（桌面磁盘缓存 1 天）；
//! - SGP4 按场景时间（SimClock，2026-09-12 04:00Z 基点）传播，TEME → GMST 旋转 →
//!   地固经纬高 → 球面标记（暂停即静止，倍速快进）；
//! - ISS 高亮 + 名称标签 + 未来一圈的点状轨道线；
//! - S 键开关卫星层。

use std::sync::Arc;

use bevy::asset::Assets;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::math::Vec3;
use bevy::pbr::{MeshMaterial3d, StandardMaterial};
use bevy::prelude::*;
use bevy::tasks::{futures::now_or_never, IoTaskPool, Task};

use crate::globe::{lat_lon_to_vec3, GlobeCamera, GLOBE_RADIUS};
use crate::sim::SimClock;

/// 场景时间基点：2026-09-12T04:00:00Z
pub const SCENARIO_EPOCH_UNIX: f64 = 1_789_185_600.0;
const TLE_ENDPOINT: &str = "https://celestrak.org/NORAD/elements/gp.php";
/// TLE 组（空间站 + 最亮目视卫星）
const TLE_GROUPS: [&str; 2] = ["stations", "visual"];
/// ISS NORAD 编号（高亮 + 轨道线）
pub const ISS_NORAD_ID: u64 = 25544;
/// ISS 轨道线采样：一圈约 92.9 分钟
const ORBIT_SAMPLES: usize = 64;
const ORBIT_SPAN_MIN: f64 = 93.0;

// ---------- 轨道数学（纯函数，可测） ----------

/// TLE 行 1 的 epoch（列 19–32: YYDDD.DDDDDDDD）→ Unix 秒
pub fn tle_epoch_unix(line1: &str) -> Option<f64> {
    let b = line1.as_bytes();
    if b.len() < 32 {
        return None;
    }
    let yy: i64 = line1.get(18..20)?.parse().ok()?;
    let year = if yy >= 57 { 1900 + yy } else { 2000 + yy };
    let doy: f64 = line1.get(20..32)?.trim().parse().ok()?;
    Some(year_start_unix(year)? + (doy - 1.0) * 86_400.0)
}

/// 公历年 1 月 1 日 0 点 UTC → Unix 秒（1970–2099 足够）
pub fn year_start_unix(year: i64) -> Option<f64> {
    let leap = |y: i64| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days: i64 = (1970..year).map(|y| if leap(y) { 366 } else { 365 }).sum();
    Some((days * 86_400) as f64)
}

/// 格林尼治平恒星时（弧度）。unix 秒。
/// IAU 1982 展开的前两项（角秒级精度，可视化足够）
pub fn gmst_rad(unix: f64) -> f64 {
    let d = unix / 86_400.0 - 10_957.5; // 自 J2000.0（2000-01-01 12:00Z）的天数
    (280.46061837 + 360.98564736629 * d).to_radians()
}

/// TEME 位置(km) + 观测时刻 → 地固经纬高（球地球近似，可视化足够）
pub fn teme_to_geodetic(pos: [f64; 3], unix: f64) -> (f64, f64, f64) {
    let theta = gmst_rad(unix);
    let (s, c) = theta.sin_cos();
    let x_e = pos[0] * c + pos[1] * s;
    let y_e = -pos[0] * s + pos[1] * c;
    let z_e = pos[2];
    let r = (x_e * x_e + y_e * y_e + z_e * z_e).sqrt();
    let lat = (z_e / r).asin() / std::f64::consts::PI * 180.0;
    let mut lon = y_e.atan2(x_e).to_degrees();
    if lon > 180.0 {
        lon -= 360.0;
    }
    let alt = r - 6_371.0;
    (lat, lon, alt)
}

// ---------- TLE 解析 ----------

pub struct SatSpec {
    pub norad_id: u64,
    pub name: String,
    pub constants: sgp4::Constants,
    pub epoch_unix: f64,
}

impl SatSpec {
    /// 场景时刻 → 球面位置（本引擎世界坐标）
    pub fn position_at(&self, unix: f64) -> Option<Vec3> {
        let minutes = (unix - self.epoch_unix) / 60.0;
        let pred = self.constants.propagate(sgp4::MinutesSinceEpoch(minutes)).ok()?;
        let (lat, lon, alt) = teme_to_geodetic(pred.position, unix);
        let radius = GLOBE_RADIUS as f64 * (1.0 + (alt * 1000.0 / GLOBE_RADIUS as f64)).max(0.0);
        Some(lat_lon_to_vec3(lat as f32, lon as f32, radius as f32))
    }
}

pub struct SatelliteSet {
    pub sats: Vec<SatSpec>,
    pub iss_index: Option<usize>,
}

/// 解析三行 TLE 组文本（名字行 + 两行根数）
pub fn parse_tle_group(text: &str, out: &mut Vec<SatSpec>) {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut i = 0;
    while i + 2 < lines.len() {
        let name = lines[i].trim();
        let (Some(l1), Some(l2)) = (lines.get(i + 1), lines.get(i + 2)) else {
            break;
        };
        if !l1.starts_with('1') || !l2.starts_with('2') {
            i += 1;
            continue;
        }
        if let (Some(epoch), Ok(el)) = (tle_epoch_unix(l1), sgp4::Elements::from_tle(Some(name.to_owned()), l1.as_bytes(), l2.as_bytes())) {
            if let Ok(constants) = sgp4::Constants::from_elements(&el) {
                let norad = l1.get(2..7).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                out.push(SatSpec { norad_id: norad, name: name.to_owned(), constants, epoch_unix: epoch });
            }
        }
        i += 3;
    }
}

// ---------- 下载 ----------

#[cfg(not(target_arch = "wasm32"))]
async fn fetch_tle_group(group: &str) -> Result<String, String> {
    let url = format!("{TLE_ENDPOINT}?GROUP={group}&FORMAT=tle");
    let config = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build();
    let agent = config.new_agent();
    let resp = agent.get(&url).call().map_err(|e| format!("{e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.into_body()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| format!("读取失败: {e}"))
}

#[cfg(target_arch = "wasm32")]
async fn fetch_tle_group(group: &str) -> Result<String, String> {
    let url = format!("{TLE_ENDPOINT}?GROUP={group}&FORMAT=tle");
    // 按天缓存：TLE 每日更新，CelesTrak 对高频请求限流（实测 403）
    let bytes = crate::web_cache::cached_fetch_daily("tle", &url).await?;
    String::from_utf8(bytes).map_err(|e| format!("非 UTF-8: {e}"))
}

pub async fn fetch_satellite_set() -> Option<SatelliteSet> {
    let mut sats = Vec::new();
    for g in TLE_GROUPS {
        if let Ok(text) = fetch_tle_group(g).await {
            parse_tle_group(&text, &mut sats);
        }
    }
    if sats.is_empty() {
        return None;
    }
    let iss_index = sats.iter().position(|s| s.norad_id == ISS_NORAD_ID);
    Some(SatelliteSet { iss_index, sats })
}

// ---------- Bevy 集成 ----------

#[derive(Component)]
pub struct SatMarker {
    pub idx: usize,
}
#[derive(Component)]
pub struct SatLabel {
    #[allow(dead_code)] // 预留：多卫星标签
    pub idx: usize,
}
#[derive(Component)]
pub struct OrbitDot {
    pub step: usize,
}

#[derive(Resource, Default)]
pub struct SatLayer {
    pub visible: bool,
    set: Option<Arc<SatelliteSet>>,
    fetch: Option<Task<Option<SatelliteSet>>>,
    /// 失败后的下次重试时刻（elapsed 秒）；请求进行中为无穷大。
    /// 此前是一次性 tried 标志——TLE 一次网络失败后整个会话不再尝试。
    retry_at: f32,
}

/// TLE 拉取失败后的重试间隔
const TLE_RETRY_SECS: f32 = 30.0;

impl SatLayer {
    pub fn sat_count(&self) -> usize {
        self.set.as_ref().map(|s| s.sats.len()).unwrap_or(0)
    }
}

/// 球面标记的世界半径：屏幕恒定像素（与 globe 标记同族公式）
/// 在 XY 平面追加一个矩形（billboard 图标部件）
fn push_rect(pos: &mut Vec<[f32; 3]>, idx: &mut Vec<u32>, x0: f32, y0: f32, x1: f32, y1: f32) {
    let base = pos.len() as u32;
    pos.extend_from_slice(&[
        [x0, y0, 0.0],
        [x1, y0, 0.0],
        [x1, y1, 0.0],
        [x0, y1, 0.0],
    ]);
    idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// MIL-STD-2525D 语义的卫星剪影（中央本体 + 左右太阳翼），XY 平面单位框内
fn satellite_icon_mesh() -> Mesh {
    let mut pos: Vec<[f32; 3]> = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    push_rect(&mut pos, &mut idx, -0.09, -0.17, 0.09, 0.17); // 本体
    push_rect(&mut pos, &mut idx, -0.5, -0.08, -0.16, 0.08); // 左翼
    push_rect(&mut pos, &mut idx, 0.16, -0.08, 0.5, 0.08); // 右翼
    let mut m = Mesh::new(bevy::render::mesh::PrimitiveTopology::TriangleList, bevy::asset::RenderAssetUsages::default());
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    m.insert_indices(bevy::render::mesh::Indices::U32(idx));
    m
}

/// ISS 空间站剪影（横桁架 + 中央模块 + 左右成对翼板）
fn iss_icon_mesh() -> Mesh {
    let mut pos: Vec<[f32; 3]> = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    push_rect(&mut pos, &mut idx, -0.5, -0.035, 0.5, 0.035); // 主桁架
    push_rect(&mut pos, &mut idx, -0.06, -0.15, 0.06, 0.15); // 居住模块
    push_rect(&mut pos, &mut idx, -0.42, -0.15, -0.14, -0.05); // 左下翼
    push_rect(&mut pos, &mut idx, 0.14, 0.05, 0.42, 0.15); // 右上翼
    let mut m = Mesh::new(bevy::render::mesh::PrimitiveTopology::TriangleList, bevy::asset::RenderAssetUsages::default());
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    m.insert_indices(bevy::render::mesh::Indices::U32(idx));
    m
}

fn sat_marker_radius(cam_distance: f32, px: f32, viewport_h: f32) -> f32 {
    (cam_distance * px * 2.0 * (crate::globe::GLOBE_FOV * 0.5).tan() / viewport_h.max(1.0)).max(1.0)
}

/// 拉取 + 生成标记/标签/轨道点实体（Globe 态）
#[allow(clippy::type_complexity)]
pub fn sat_stream_system(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut layer: ResMut<SatLayer>,
    app: Res<bevy::state::state::State<crate::globe::AppState>>,
    clock: Res<SimClock>,
    cams: Query<&Transform, With<GlobeCamera>>,
    window: Query<&bevy::window::Window, bevy::ecs::query::With<bevy::window::PrimaryWindow>>,
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mut q_markers: Query<(&mut Transform, &SatMarker), (Without<SatLabel>, Without<OrbitDot>, Without<GlobeCamera>)>,
    mut q_labels: Query<(&mut Transform, &mut Visibility, &mut Text2d), (With<SatLabel>, Without<SatMarker>, Without<GlobeCamera>)>,
    mut q_orbit: Query<(&mut Transform, &mut Visibility, &OrbitDot), (Without<SatMarker>, Without<SatLabel>, Without<GlobeCamera>)>,
    mut q_vis: Query<&mut Visibility, (With<SatMarker>, Without<SatLabel>, Without<OrbitDot>)>,
) {
    // ---- 地图态（OSM）下卫星层整体关闭：3D 图标本就不可见，
    // 但 ISS 的 Text2d 标签走 2D 管线，必须显式隐藏防泄漏到地图上 ----
    if *app.get() != crate::globe::AppState::Globe {
        for mut v in &mut q_vis {
            *v = Visibility::Hidden;
        }
        for (_, mut v, _) in &mut q_labels {
            *v = Visibility::Hidden;
        }
        for (_, mut v, _) in &mut q_orbit {
            *v = Visibility::Hidden;
        }
        return;
    }

    // ---- S 键开关 ----
    if keys.just_pressed(KeyCode::KeyS) {
        layer.visible = !layer.visible;
    }
    // ---- 拉取 TLE（失败后定期重试） ----
    if layer.set.is_none() {
        if let Some(task) = layer.fetch.as_mut() {
            if let Some(result) = now_or_never(&mut *task) {
                layer.fetch = None;
                if let Some(set) = result {
                    eprintln!("[sat] 加载 {} 颗卫星（ISS index {:?}）", set.sats.len(), set.iss_index);
                    layer.set = Some(Arc::new(set));
                } else {
                    eprintln!("[sat] TLE 获取失败，{}s 后重试", TLE_RETRY_SECS);
                    layer.retry_at = time.elapsed_secs() + TLE_RETRY_SECS;
                }
            }
        } else if time.elapsed_secs() >= layer.retry_at {
            layer.retry_at = f32::INFINITY; // 请求进行中
            layer.visible = true;
            layer.fetch = Some(IoTaskPool::get().spawn(async { fetch_satellite_set().await }));
            return;
        } else {
            return;
        }
    }
    let Some(set) = layer.set.clone() else { return };
    let Ok(cam_t) = cams.single() else { return };
    let vp_h = window.single().map(|w| w.height()).unwrap_or(900.0);

    // 首次有数据：生成实体
    if q_markers.is_empty() {
        let dot = meshes.add(Mesh::from(bevy::math::primitives::Sphere::new(1.0)));
        let icon_std = meshes.add(satellite_icon_mesh());
        let icon_iss = meshes.add(iss_icon_mesh());
        let mat_iss = materials.add(StandardMaterial {
            base_color: bevy::color::Color::srgb_u8(255, 224, 109),
            unlit: true,
            cull_mode: None,
            ..default()
        });
        let mat_std = materials.add(StandardMaterial {
            base_color: bevy::color::Color::srgb_u8(94, 234, 212),
            unlit: true,
            cull_mode: None,
            ..default()
        });
        for (idx, s) in set.sats.iter().enumerate() {
            let is_iss = Some(idx) == set.iss_index;
            commands.spawn((
                bevy::mesh::Mesh3d(if is_iss { icon_iss.clone() } else { icon_std.clone() }),
                MeshMaterial3d(if is_iss { mat_iss.clone() } else { mat_std.clone() }),
                Transform::default(),
                Visibility::Visible,
                SatMarker { idx },
            ));
            if is_iss {
                commands.spawn((
                    Text2d::new(s.name.clone()),
                    TextColor(crate::map_render::palette::SELECT),
                    Transform::default(),
                    Visibility::Visible,
                    SatLabel { idx },
                ));
                for step in 0..ORBIT_SAMPLES {
                    commands.spawn((
                        bevy::mesh::Mesh3d(dot.clone()),
                        MeshMaterial3d(mat_iss.clone()),
                        Transform::default(),
                        Visibility::Visible,
                        OrbitDot { step },
                    ));
                }
            }
        }
    }

    // ---- 位置更新（每帧；SGP4 160 颗 <1ms） ----
    let unix = SCENARIO_EPOCH_UNIX + clock.t;
    let cam_dist = cam_t.translation.length();
    let r = sat_marker_radius(cam_dist, 7.0, vp_h);
    let orbit_r = sat_marker_radius(cam_dist, 2.5, vp_h);
    // 近距（接近落地阈值）时卫星层密集碍事，随视距自动淡出
    let near_hide = GLOBE_RADIUS * 1.15;
    let show = layer.visible && cam_dist > near_hide;
    let label_show = show && cam_dist < GLOBE_RADIUS * 1.6;
    let cam_up = cam_t.up();
    for (mut t, m) in &mut q_markers {
        if let Some(p) = set.sats[m.idx].position_at(unix) {
            t.translation = p;
            t.scale = Vec3::splat(r * 1.6);
            // 剪影图标需始终面向相机（billboard）；up 取相机上方向避免极区退化
            t.look_at(cam_t.translation, cam_up);
        }
    }
    for (mut t, mut vis, mut text) in &mut q_labels {
        if let Some(idx) = set.iss_index {
            if let Some(p) = set.sats[idx].position_at(unix) {
                t.translation = p + Vec3::Y * r * 2.5;
                t.scale = Vec3::splat(cam_dist * 11.0 * 2.0 * (crate::globe::GLOBE_FOV * 0.5).tan() / vp_h.max(1.0));
                text.0 = set.sats[idx].name.clone();
            }
        }
        *vis = if label_show { Visibility::Visible } else { Visibility::Hidden };
    }
    if let Some(idx) = set.iss_index {
        for (mut t, mut v, o) in &mut q_orbit {
            let unix_f = unix + ORBIT_SPAN_MIN * 60.0 * o.step as f64 / ORBIT_SAMPLES as f64;
            if let Some(p) = set.sats[idx].position_at(unix_f) {
                t.translation = p;
                t.scale = Vec3::splat(orbit_r);
            }
            *v = if show { Visibility::Visible } else { Visibility::Hidden };
        }
    }
    // ---- 层可见性 ----
    for mut v in &mut q_vis {
        *v = if show { Visibility::Visible } else { Visibility::Hidden };
    }
    let _ = time.delta();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tle_epoch_parse() {
        // ISS 2026-09-14 03:26:29Z（年积日 257.14321681）
        let l1 = "1 25544U 98067A   26257.14321681  .00004666  00000+0  92466-4 0  9990";
        let unix = tle_epoch_unix(l1).unwrap();
        // 验算：2026-01-01 = year_start_unix(2026)；+256.14321681 天
        let ys = year_start_unix(2026).unwrap();
        assert!((unix - ys - 256.14321681 * 86_400.0).abs() < 1.0);
        // year_start 基准：2026-01-01 = 1767225600（UTC）
        assert_eq!(ys as i64, 1_767_225_600);
    }

    #[test]
    fn gmst_j2000() {
        // J2000.0（2000-01-01 12:00Z）GMST ≈ 18h41m50.5s ≈ 280.46°
        let unix = 946_728_000.0;
        let g = gmst_rad(unix).to_degrees();
        assert!((g - 280.46).abs() < 0.05, "GMST = {g}");
    }

    #[test]
    fn teme_roundtrip_sane() {
        // 格林尼治子午线上空 1000km：θ 相消，lon≈0
        let theta = gmst_rad(1_000_000_000.0);
        let x = 7371.0 * theta.cos();
        let y = 7371.0 * theta.sin();
        let (lat, lon, alt) = teme_to_geodetic([x, y, 0.0], 1_000_000_000.0);
        assert!(lat.abs() < 0.01, "lat = {lat}");
        assert!(lon.abs() < 0.01, "lon = {lon}");
        assert!((alt - 1000.0).abs() < 1.0, "alt = {alt}");
    }

    #[test]
    fn propagate_at_scenario_epoch() {
        // 场景时刻（早于 TLE epoch 约 2 天，负分钟传播）
        let l1 = "1 25544U 98067A   26257.14321681  .00004666  00000+0  92466-4 0  9990";
        let l2 = "2 25544  51.6309 219.8284 0004930 139.3822 220.7535 15.49107075585544";
        let el = sgp4::Elements::from_tle(Some("ISS".into()), l1.as_bytes(), l2.as_bytes()).unwrap();
        let constants = sgp4::Constants::from_elements(&el).unwrap();
        let spec = SatSpec { norad_id: 25544, name: "ISS".into(), constants, epoch_unix: tle_epoch_unix(l1).unwrap() };
        let p = spec.position_at(SCENARIO_EPOCH_UNIX).expect("场景时刻传播失败");
        let ratio = p.length() / GLOBE_RADIUS;
        assert!(ratio > 1.03 && ratio < 1.10, "ISS 轨道高度比 {ratio}");
        // 90 分钟后仍在轨道上
        let p2 = spec.position_at(SCENARIO_EPOCH_UNIX + 5400.0).expect("半圈后传播失败");
        let r2 = p2.length() / GLOBE_RADIUS;
        assert!(r2 > 1.03 && r2 < 1.10, "半圈后高度比 {r2}");
    }

    #[test]
    fn tle_group_parse_and_propagate() {
        let text = "ISS (ZARYA)             \n1 25544U 98067A   26257.14321681  .00004666  00000+0  92466-4 0  9990\n2 25544  51.6309 219.8284 0004930 139.3822 220.7535 15.49107075585544\nPOISK\n1 36086U 09060A   26257.14321681  .00004666  00000+0  92466-4 0  9998\n2 36086  51.6309 219.8284 0004930 139.3822 220.7535 15.49107075586100\n";
        let mut sats = Vec::new();
        parse_tle_group(text, &mut sats);
        assert_eq!(sats.len(), 2);
        assert_eq!(sats[0].norad_id, 25544);
        assert_eq!(sats[1].norad_id, 36086);
        // TLE epoch 处传播：高度应在 LEO（300-600km）
        let p = sats[0].position_at(sats[0].epoch_unix).unwrap();
        let alt_world = p.length() / GLOBE_RADIUS;
        assert!(alt_world > 1.03 && alt_world < 1.1, "ISS 高度异常: {alt_world}");
        // 位置非零且在球外
        assert!(p.length() > GLOBE_RADIUS);
    }
}

