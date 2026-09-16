//! 地球（全球态势）视图与 地球 ↔ 战术地图 视图切换。
//!
//! - 自生成等距圆柱 UV 球体，保证地球贴图与经纬度标记严格对齐；
//! - 地球上推进到近地阈值时，按“视角连续性”公式把地表视距换算成地图的
//!   米/像素，直接落入战术图，缩放手感连续；
//! - 地图向外缩放到极限则自动“升轨”回到地球，相机对准原视野中心；
//! - 单位以战略标记（小圆点）同步显示在球面上，遵循战争迷雾；
//!   OSM 数据区以黄色点环标出（当前为珍珠港）。

use bevy::asset::{Assets, Handle, RenderAssetUsages};
use bevy::camera::{ImageRenderTarget, RenderTarget};
use bevy::color::Color;
use bevy::ecs::entity::Entity;
use bevy::ecs::resource::Resource;
use bevy::state::state::{NextState, States};
use bevy::ecs::system::{Commands, Local, Query, Res, ResMut};
use bevy::input::mouse::{AccumulatedMouseScroll, MouseButton};
use bevy::math::{Vec2, Vec3};
use bevy::pbr::{MeshMaterial3d, StandardMaterial};
use bevy::prelude::*;
use bevy::render::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::window::{PrimaryWindow, Window};

use crate::camera::{CameraRig, CursorState};
use crate::map_render::palette;
use crate::sim::{Position, Side, Unit};
use crate::{MapCtx, RenderMode};

pub const GLOBE_RADIUS: f32 = 6_371_000.0;
/// 地图侧允许的最大米/像素（缩放到此即升轨回地球）
pub const MAP_MAX_MPP: f32 = 1200.0;
/// 地球侧推进到此地表视距即落入地图（留迟滞避免抖动）
pub const LAND_MPP: f32 = 1120.0;
/// 地球相机垂直视场角
pub const GLOBE_FOV: f32 = std::f32::consts::FRAC_PI_4;

// ---------- 视图状态机 ----------

#[derive(States, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AppState {
    #[default]
    Globe,
    Map,
}

// ---------- 坐标换算 ----------

/// 经纬度（度）→ 球面位置。lon 向东为正；与 globe_sphere_mesh 的 UV 严格一致。
pub fn lat_lon_to_vec3(lat_deg: f32, lon_deg: f32, radius: f32) -> Vec3 {
    let lat = lat_deg.to_radians();
    let lon = lon_deg.to_radians();
    Vec3::new(
        radius * lat.cos() * lon.cos(),
        radius * lat.sin(),
        -radius * lat.cos() * lon.sin(),
    )
}

/// 球面位置 → 经纬度（度）
#[allow(dead_code)] // 测试与未来地球拾取使用
pub fn vec3_to_lat_lon(v: Vec3) -> (f32, f32) {
    let n = v.normalize_or_zero();
    (n.y.asin().to_degrees(), (-n.z).atan2(n.x).to_degrees())
}

/// 地表视距（相机到球心距离）在视口下呈现的“米/像素”
pub fn apparent_mpp(distance_to_center: f32, viewport_h: f32) -> f32 {
    ((distance_to_center - GLOBE_RADIUS) * 2.0 * (GLOBE_FOV * 0.5).tan() / viewport_h.max(1.0))
        .max(0.3)
}

/// 逆换算：给定米/像素求相机到球心距离
pub fn distance_for_mpp(mpp: f32, viewport_h: f32) -> f32 {
    GLOBE_RADIUS
        + (mpp * viewport_h.max(1.0) / (2.0 * (GLOBE_FOV * 0.5).tan())).max(GLOBE_RADIUS * 0.02)
}

// ---------- 球体网格 ----------

/// 等距圆柱 UV 球体：u=(lon+180)/360，v=(90-lat)/180，与标准地球贴图对齐。
pub fn globe_sphere_mesh(radius: f32, lon_segments: usize, lat_segments: usize) -> Mesh {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    for i in 0..=lat_segments {
        let v = i as f32 / lat_segments as f32;
        let lat = 90.0 - 180.0 * v;
        for j in 0..=lon_segments {
            let u = j as f32 / lon_segments as f32;
            let lon = -180.0 + 360.0 * u;
            let p = lat_lon_to_vec3(lat, lon, radius);
            positions.push(p.to_array());
            normals.push(p.normalize_or_zero().to_array());
            uvs.push([u, v]);
        }
    }
    let w = lon_segments + 1;
    let mut indices: Vec<u32> = Vec::new();
    for i in 0..lat_segments {
        for j in 0..lon_segments {
            let a = (i * w + j) as u32;
            let b = a + 1;
            let c = a + w as u32;
            let d = c + 1;
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

// ---------- 资源与组件 ----------

/// 地球相机姿态：相机所在经纬度 + 到球心距离
#[derive(Resource)]
pub struct GlobeRig {
    pub lat: f32,
    pub lon: f32,
    pub distance: f32,
}

impl Default for GlobeRig {
    fn default() -> Self {
        // 初始对准珍珠港，全球视角
        GlobeRig { lat: 21.355, lon: -157.925, distance: GLOBE_RADIUS * 2.9 }
    }
}

/// 标记地球相机（窗口目标或离屏图像目标）
#[derive(Component)]
pub struct GlobeCamera;

/// 球面上的单位战略标记
#[derive(Component)]
pub struct GlobeMarker {
    pub unit: Entity,
}

/// OSM 数据区边界点

/// 已加载 OSM 数据区（南、西、北、东），实时瓦片流更新，地球黄环跟随
#[derive(Resource, Default)]
pub struct DataRing {
    pub bbox: Option<(f64, f64, f64, f64)>,
}

/// 地球贴图（构建期内嵌，wasm/桌面通用）
#[derive(Resource)]
pub struct EarthTexture(pub Handle<bevy::image::Image>);

/// 底图球实体标记（瓦片层就绪后隐藏，避免与 GIBS 瓦片混贴）
#[derive(Component)]
pub struct BaseGlobe;

#[derive(Resource)]
pub struct GlobeVisuals {
    pub marker_mesh: Handle<Mesh>,
    pub mat_blue: Handle<StandardMaterial>,
    pub mat_red: Handle<StandardMaterial>,
    pub mat_yellow: Handle<StandardMaterial>,
    pub mat_neutral: Handle<StandardMaterial>,
}

// ---------- 场景构建 ----------

/// 生成地球场景：球体、OSM 数据区点环、相机。
/// `image_target` 为 Some 时相机渲染到离屏图像（--render-globe 验证用）。
pub fn setup_globe(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    std_materials: &mut Assets<StandardMaterial>,
    earth: &EarthTexture,
    image_target: Option<Handle<bevy::image::Image>>,
) {
    let earth_mat = std_materials.add(StandardMaterial {
        base_color_texture: Some(earth.0.clone()),
        // 与 GIBS 瓦片层同款冷色 tint：瓦片缺失露出底图时色调一致，不再突兀
        base_color: Color::srgb(0.52, 0.60, 0.72),
        unlit: true,
        cull_mode: None,
        ..default()
    });
    let sphere = meshes.add(globe_sphere_mesh(GLOBE_RADIUS * 0.985, 96, 48));
    commands.spawn((
        Mesh3d(sphere),
        MeshMaterial3d(earth_mat),
        Transform::default(),
        Visibility::default(),
        BaseGlobe,
    ));

    let unlit = |c: Color| StandardMaterial { base_color: c, unlit: true, cull_mode: None, ..default() };
    let dot_mesh = meshes.add(Mesh::from(Sphere::new(1.0)));
    let visuals = GlobeVisuals {
        marker_mesh: dot_mesh.clone(),
        mat_blue: std_materials.add(unlit(palette::SIDE_BLUE)),
        mat_red: std_materials.add(unlit(palette::SIDE_RED)),
        mat_yellow: std_materials.add(unlit(palette::SELECT)),
        mat_neutral: std_materials.add(unlit(palette::SIDE_NEUTRAL)),
    };
    commands.insert_resource(visuals);


    // 相机（窗口或离屏图像目标）
    let mut cam = commands.spawn((
        Camera3d::default(),
        // unlit 暗色风格无需电影级色调映射（也省去 tonemapping_luts 特性）
        bevy::core_pipeline::tonemapping::Tonemapping::None,
        GlobeCamera,
        Camera { is_active: true, ..default() },
        // WebGL2 下 MSAA 破坏同相机 UI pass，相机级关闭
        Msaa::Off,
        // UI 渲染挂活跃相机：Globe 态由 3D 相机承载（切换见 enter_*_cameras）
        bevy::ui::IsDefaultUiCamera,
        Projection::Perspective(PerspectiveProjection {
            fov: GLOBE_FOV,
            near: 10_000.0,
            far: 40_000_000.0,
            ..default()
        }),
        Transform::default(),
    ));
    if let Some(img) = image_target {
        cam.insert(RenderTarget::Image(ImageRenderTarget { handle: img, scale_factor: 1.0 }));
    }
}

// ---------- 单位标记 ----------

pub fn spawn_globe_markers(
    new_units: Query<(Entity, &Unit), bevy::ecs::query::Added<Unit>>,
    visuals: Option<Res<GlobeVisuals>>,
    mut commands: Commands,
) {
    let Some(v) = visuals else { return };
    for (e, _u) in &new_units {
        commands.spawn((
            Mesh3d(v.marker_mesh.clone()),
            MeshMaterial3d(v.mat_blue.clone()),
            Transform::default(),
            Visibility::Hidden,
            GlobeMarker { unit: e },
        ));
    }
}

/// 球面标记的世界半径：屏幕恒定像素
fn marker_world_radius(cam_distance: f32, px: f32, viewport_h: f32) -> f32 {
    (cam_distance * px * 2.0 * (GLOBE_FOV * 0.5).tan() / viewport_h.max(1.0)).max(1.0)
}

/// 同步单位标记（位置/像素恒定缩放/迷雾显隐/阵营配色）
pub fn sync_globe_markers(
    units: Query<(&Unit, &Position)>,
    visuals: Option<Res<GlobeVisuals>>,
    ctx: Res<MapCtx>,
    cams: Query<&Transform, With<GlobeCamera>>,
    window: Query<&Window, With<PrimaryWindow>>,
    mut q: Query<
        (&GlobeMarker, &mut Transform, &mut Visibility, &mut MeshMaterial3d<StandardMaterial>),
        Without<GlobeCamera>,
    >,
) {
    let (Ok(cam_t), Some(visuals)) = (cams.single(), visuals.as_deref()) else { return };
    let vp_h = window.single().map(|w| w.height()).unwrap_or(900.0);
    for (m, mut t, mut vis, mut mat) in &mut q {
        let Ok((u, p)) = units.get(m.unit) else {
            *vis = Visibility::Hidden;
            continue;
        };
        *vis = match u.side {
            Side::Red if !u.detected_by_blue => Visibility::Hidden,
            _ => Visibility::Visible,
        };
        let (lat, lon) = ctx.proj.unproject(p.0);
        let pos = lat_lon_to_vec3(lat as f32, lon as f32, GLOBE_RADIUS * 1.003);
        t.translation = pos;
        let r = marker_world_radius(cam_t.translation.distance(pos), 9.0, vp_h);
        t.scale = Vec3::splat(r);
        mat.0 = match u.side {
            Side::Red if !u.classified => visuals.mat_yellow.clone(),
            Side::Red => visuals.mat_red.clone(),
            Side::Neutral => visuals.mat_neutral.clone(),
            Side::Blue => visuals.mat_blue.clone(),
        };
    }
}

// ---------- 相机与控制 ----------

pub fn sync_globe_camera(rig: Res<GlobeRig>, mut cam: Query<&mut Transform, With<GlobeCamera>>) {
    let Ok(mut t) = cam.single_mut() else { return };
    t.translation = lat_lon_to_vec3(rig.lat, rig.lon, rig.distance);
    t.look_at(Vec3::ZERO, Vec3::Y);
}

#[derive(Default)]
pub(crate) struct DragState {
    down: bool,
    last: Vec2,
}

/// 地球控制：拖拽旋转、滚轮推进、右键/G 落入地图；推过阈值自动落入。
pub fn globe_controls(
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Res<CursorState>,
    scroll: Res<AccumulatedMouseScroll>,
    keys: Res<ButtonInput<KeyCode>>,
    mut rig: ResMut<GlobeRig>,
    mut map_rig: ResMut<CameraRig>,
    ctx: Res<MapCtx>,
    window: Query<&Window, With<PrimaryWindow>>,
    mut next: ResMut<NextState<AppState>>,
    mut drag: Local<DragState>,
) {
    let vp_h = window.single().map(|w| w.height()).unwrap_or(900.0);

    // 拖拽旋转（抓球感：越近转越快）
    if buttons.pressed(MouseButton::Left) {
        if let Some(c) = cursor.screen {
            if !drag.down {
                drag.down = true;
                drag.last = c;
            } else {
                let d = c - drag.last;
                let rate = 0.55 * (rig.distance / GLOBE_RADIUS - 1.0).max(0.02);
                rig.lon -= d.x * rate;
                rig.lat = (rig.lat + d.y * rate).clamp(-85.0, 85.0);
                drag.last = c;
            }
        }
    } else {
        drag.down = false;
    }

    // 滚轮推进
    if scroll.delta.y.abs() > 1e-4 {
        let factor = 1.0 + scroll.delta.y.clamp(-1.0, 1.0) * 0.15;
        rig.distance = (rig.distance / factor).clamp(GLOBE_RADIUS * 1.02, GLOBE_RADIUS * 4.0);
    }

    // 右键 / G：立即落入当前注视点；推进过阈值也自动落入
    let land_now = buttons.just_pressed(MouseButton::Right) || keys.just_pressed(KeyCode::KeyG);
    if land_now || apparent_mpp(rig.distance, vp_h) < LAND_MPP {
        map_rig.target = ctx.proj.project(rig.lat as f64, rig.lon as f64);
        map_rig.mpp = LAND_MPP;
        next.set(AppState::Map);
    }
}

/// 地图缩放到极限 → 升轨回地球（相机对准原视野中心，视距按缩放连续换算）
pub fn map_takeoff(
    rig: Res<CameraRig>,
    ctx: Res<MapCtx>,
    mode: Res<RenderMode>,
    mut globe: ResMut<GlobeRig>,
    window: Query<&Window, With<PrimaryWindow>>,
    mut next: ResMut<NextState<AppState>>,
) {
    if *mode != RenderMode::Window || rig.mpp < MAP_MAX_MPP - 1.0 {
        return;
    }
    let (lat, lon) = ctx.proj.unproject(rig.target);
    let vp_h = window.single().map(|w| w.height()).unwrap_or(900.0);
    globe.lat = lat as f32;
    globe.lon = lon as f32;
    globe.distance = distance_for_mpp(MAP_MAX_MPP * 1.06, vp_h);
    next.set(AppState::Globe);
}

// ---------- 相机激活 ----------

pub fn enter_globe_cameras(
    mut commands: Commands,
    mut q2d: Query<(Entity, &mut Camera), (With<Camera2d>, Without<GlobeCamera>)>,
    mut q3d: Query<(Entity, &mut Camera), With<GlobeCamera>>,
) {
    for (e, mut c) in &mut q2d {
        c.is_active = false;
        commands.entity(e).remove::<bevy::ui::IsDefaultUiCamera>();
    }
    for (e, mut c) in &mut q3d {
        c.is_active = true;
        commands.entity(e).insert(bevy::ui::IsDefaultUiCamera);
    }
}

pub fn enter_map_cameras(
    mut commands: Commands,
    mut q2d: Query<(Entity, &mut Camera), (With<Camera2d>, Without<GlobeCamera>)>,
    mut q3d: Query<(Entity, &mut Camera), With<GlobeCamera>>,
) {
    for (e, mut c) in &mut q2d {
        c.is_active = true;
        commands.entity(e).insert(bevy::ui::IsDefaultUiCamera);
    }
    for (e, mut c) in &mut q3d {
        c.is_active = false;
        commands.entity(e).remove::<bevy::ui::IsDefaultUiCamera>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lat_lon_vec3_roundtrip() {
        for &(lat, lon) in &[(0.0, 0.0), (21.355, -157.925), (-33.86, 151.21), (65.0, -30.0)] {
            let v = lat_lon_to_vec3(lat, lon, GLOBE_RADIUS);
            let (lat2, lon2) = vec3_to_lat_lon(v);
            assert!((lat2 - lat).abs() < 1e-3, "lat: {lat} -> {lat2}");
            assert!((lon2 - lon).abs() < 1e-3, "lon: {lon} -> {lon2}");
        }
    }

    #[test]
    fn lat_lon_axes() {
        // 本初子午线赤道在 +X，东经 90 在 -Z，北极在 +Y
        assert!(lat_lon_to_vec3(0.0, 0.0, 1.0).x > 0.999);
        let e90 = lat_lon_to_vec3(0.0, 90.0, 1.0);
        assert!(e90.z < -0.999 && e90.x.abs() < 1e-3);
        assert!(lat_lon_to_vec3(90.0, 0.0, 1.0).y > 0.999);
    }

    #[test]
    fn sphere_mesh_shape() {
        let mesh = globe_sphere_mesh(1.0, 24, 12);
        let n = 25 * 13;
        assert_eq!(mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().len(), n);
        assert_eq!(mesh.attribute(Mesh::ATTRIBUTE_UV_0).unwrap().len(), n);
        assert_eq!(mesh.indices().unwrap().len(), 24 * 12 * 6);
        // 所有顶点半径为 1
        if let Some(values) = mesh.attribute(Mesh::ATTRIBUTE_POSITION) {
            for v in values.as_float3().unwrap() {
                let r = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                assert!((r - 1.0).abs() < 1e-4, "radius = {r}");
            }
        }
    }

    #[test]
    fn mpp_distance_roundtrip() {
        let vp = 900.0_f32;
        // 低于地表高度下限（2%R ≈ 127km，对应 mpp≈117）时被截断，只测有效区间
        for &mpp in &[200.0, 300.0, 1120.0, 1200.0] {
            let d = distance_for_mpp(mpp, vp);
            let back = apparent_mpp(d, vp);
            assert!((back - mpp).abs() < 0.5, "{mpp} -> {d} -> {back}");
        }
        // 极小 mpp 被抬升到地表下限
        assert!((distance_for_mpp(1.0, vp) - GLOBE_RADIUS) - GLOBE_RADIUS * 0.02 < 1.0);
        // 全球距离下地表视距远大于地图极限
        assert!(apparent_mpp(GLOBE_RADIUS * 2.9, vp) > MAP_MAX_MPP * 5.0);
    }
}
