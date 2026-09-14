//! 单位可视化：NTDS 风格符号、速度矢量线、标签、选择环、传感器范围圈、航线。
//!
//! 符号网格以“半径 1”建模，运行时按 `mpp` 缩放保持屏幕像素恒定；
//! 速度矢量线是真实物理长度（1 分钟航程）。

use bevy::asset::RenderAssetUsages;
use std::collections::HashMap;

use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;

use crate::camera::CameraRig;
use crate::input::Selection;
use crate::map_render::{palette, seg_quad};
use crate::sim::{Domain, Side, Unit, Heading, Position, SpeedMps};

/// 符号基准半径（屏幕像素）
pub const SYMBOL_PX: f32 = 13.0;

// ---------- 网格构建（纯函数） ----------

fn new_mesh(verts: Vec<[f32; 3]>, idx: Vec<u32>) -> Mesh {
    let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    m.insert_attribute(Mesh::ATTRIBUTE_POSITION, verts);
    m.insert_indices(Indices::U32(idx));
    m
}

/// 圆环（半径 1 外沿 / 0.72 内沿）
pub fn annulus_mesh(segs: usize) -> Mesh {
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    for i in 0..segs {
        let a0 = (i as f32) * std::f32::consts::TAU / segs as f32;
        let a1 = ((i + 1) as f32) * std::f32::consts::TAU / segs as f32;
        let o0 = Vec2::new(a0.cos(), a0.sin());
        let o1 = Vec2::new(a1.cos(), a1.sin());
        let i0 = o0 * 0.72;
        let i1 = o1 * 0.72;
        let base = verts.len() as u32;
        for v in [o0, o1, i1, i0] {
            verts.push([v.x, v.y, 0.0]);
        }
        idx.extend_from_slice(&[
            base, base + 1, base + 2, base, base + 2, base + 3, // 双面
            base, base + 2, base + 1, base, base + 3, base + 2,
        ]);
    }
    new_mesh(verts, idx)
}

// ---------- MIL-STD-2525 / APP-6 风格符号 ----------
//
// 框架按身份 × 领域：友方 空中=拱顶框 / 水面=圆角矩形 / 水下=碗底框 / 地面=矩形；
// 敌方=尖顶菱形；中立=方形菱形；未知=四叶形。框内叠加象形图标（线稿）。
// 线段统一走 seg_quad（细长四边形，双面），弧用折线近似。

const FRAME_W: f32 = 0.12; // 框架线宽（相对半径 1.0）
const ICON_W: f32 = 0.11; // 图标线宽（相对半径 1.0）

/// 折线组 → 线段网格（每组折线不闭合）
fn polylines_mesh(lines: &[&[Vec2]], width: f32) -> Mesh {
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    for line in lines {
        for pair in line.windows(2) {
            seg_quad(&pair[0], &pair[1], width, &mut verts, &mut idx);
        }
    }
    new_mesh(verts, idx)
}

fn arc_pts(cx: f32, cy: f32, r: f32, a0: f32, a1: f32, segs: usize) -> Vec<Vec2> {
    (0..=segs)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / segs as f32;
            Vec2::new(cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

fn rect_pts(w: f32, h: f32) -> Vec<Vec2> {
    vec![
        Vec2::new(-w / 2.0, -h / 2.0),
        Vec2::new(w / 2.0, -h / 2.0),
        Vec2::new(w / 2.0, h / 2.0),
        Vec2::new(-w / 2.0, h / 2.0),
        Vec2::new(-w / 2.0, -h / 2.0),
    ]
}

/// 友方·地面/设施：矩形框（宽:高 ≈ 1.6:1）
pub fn frame_land_lines() -> Vec<Vec<Vec2>> {
    vec![rect_pts(1.6, 1.0)]
}

/// 友方·空中：拱顶框（矩形下身 + 上半圆顶）
pub fn frame_air_lines() -> Vec<Vec<Vec2>> {
    let w = 1.6;
    let half = w / 2.0;
    let body_top = 0.2;
    let r = half; // 拱半径与半宽一致
    let arc = arc_pts(0.0, body_top, r, 0.0, std::f32::consts::PI, 16);
    vec![
        vec![
            Vec2::new(-half, body_top),
            Vec2::new(-half, -0.5),
            Vec2::new(half, -0.5),
            Vec2::new(half, body_top),
        ],
        arc,
    ]
}

/// 友方·水面：圆角矩形框
pub fn frame_sea_lines() -> Vec<Vec<Vec2>> {
    let w = 1.8;
    let h = 1.1;
    let r = h / 2.0;
    let (hw, hh) = (w / 2.0, h / 2.0);
    let mut pts = Vec::new();
    pts.extend(arc_pts(-hw + r, -hh + r, r, std::f32::consts::PI, 1.5 * std::f32::consts::PI, 6));
    pts.extend(arc_pts(hw - r, -hh + r, r, 1.5 * std::f32::consts::PI, 2.0 * std::f32::consts::PI, 6));
    pts.extend(arc_pts(hw - r, hh - r, r, 0.0, 0.5 * std::f32::consts::PI, 6));
    pts.extend(arc_pts(-hw + r, hh - r, r, 0.5 * std::f32::consts::PI, std::f32::consts::PI, 6));
    pts.push(pts[0]);
    vec![pts]
}

/// 友方·水下：碗底框（矩形上身 + 下半圆底）
pub fn frame_sub_lines() -> Vec<Vec<Vec2>> {
    let w = 1.5;
    let half = w / 2.0;
    let body_bottom = -0.15;
    let arc = arc_pts(0.0, body_bottom, half, std::f32::consts::PI, 2.0 * std::f32::consts::PI, 16);
    vec![
        vec![
            Vec2::new(-half, body_bottom),
            Vec2::new(-half, 0.5),
            Vec2::new(half, 0.5),
            Vec2::new(half, body_bottom),
        ],
        arc,
    ]
}

/// 中立：方形菱形（正方形旋转 45°，区别于敌方瘦高菱形）
pub fn frame_neutral_lines() -> Vec<Vec<Vec2>> {
    let d = 1.05;
    vec![vec![
        Vec2::new(0.0, d),
        Vec2::new(d, 0.0),
        Vec2::new(0.0, -d),
        Vec2::new(-d, 0.0),
        Vec2::new(0.0, d),
    ]]
}

/// 未知：四叶形（quatrefoil，四段 90° 外凸圆弧）
pub fn frame_quatrefoil_lines() -> Vec<Vec<Vec2>> {
    let r = 0.62;
    let d = 0.52;
    let mut pts = Vec::new();
    let mut centers = Vec::new();
    for k in 0..4 {
        let a = (k as f32) * std::f32::consts::FRAC_PI_2;
        centers.push(Vec2::new(a.cos(), a.sin()) * d);
    }
    for (i, c) in centers.iter().enumerate() {
        let a_start = (i as f32 + 0.5) * std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_2;
        // 每叶：以中心为圆心的一段外凸弧
        let a0 = a_start - 0.5 * std::f32::consts::FRAC_PI_2;
        let a1 = a_start + 0.5 * std::f32::consts::FRAC_PI_2;
        pts.extend(arc_pts(c.x, c.y, r, a0, a1, 8));
    }
    pts.push(pts[0]);
    vec![pts]
}

// ---------- 框内象形图标（线稿） ----------

/// 舰船：船壳折线 + 桅杆（水面舰 / 商船）
fn icon_ship() -> Vec<Vec<Vec2>> {
    vec![
        vec![
            Vec2::new(-0.55, 0.12),
            Vec2::new(-0.68, -0.18),
            Vec2::new(0.68, -0.18),
            Vec2::new(0.55, 0.12),
            Vec2::new(-0.55, 0.12),
        ],
        vec![Vec2::new(0.0, -0.18), Vec2::new(0.0, 0.42)],
        vec![Vec2::new(-0.28, 0.42), Vec2::new(0.28, 0.42)],
    ]
}

/// 潜艇：下弧艇体 + 指挥塔竖线
fn icon_submarine() -> Vec<Vec<Vec2>> {
    let mut hull = arc_pts(0.0, 0.08, 0.62, std::f32::consts::PI, 2.0 * std::f32::consts::PI, 12);
    hull.push(hull[0]);
    vec![
        hull,
        vec![Vec2::new(0.0, 0.08), Vec2::new(0.0, 0.45)],
        vec![Vec2::new(0.62, 0.08), Vec2::new(0.75, -0.12)],
    ]
}

/// 固定翼：机身线 + 后掠主翼（左/右）+ 尾翼
fn icon_fixed_wing() -> Vec<Vec<Vec2>> {
    vec![
        vec![Vec2::new(-0.62, 0.05), Vec2::new(0.62, 0.05)],
        vec![Vec2::new(0.1, 0.05), Vec2::new(0.5, 0.45)],
        vec![Vec2::new(0.1, 0.05), Vec2::new(0.5, -0.35)],
        vec![Vec2::new(-0.62, 0.05), Vec2::new(-0.38, 0.32)],
    ]
}

/// 雷达：上拱弧（拱顶朝上）+ 中心辐射线
fn icon_radar() -> Vec<Vec<Vec2>> {
    let arc = arc_pts(0.0, -0.35, 0.55, 0.0, std::f32::consts::PI, 10);
    vec![
        arc,
        vec![Vec2::new(0.0, -0.35), Vec2::new(0.42, 0.1)],
        vec![Vec2::new(0.0, -0.35), Vec2::new(-0.42, 0.1)],
        vec![Vec2::new(0.0, -0.35), Vec2::new(0.0, 0.2)],
    ]
}

/// 基地/设施：内部小旗（竖杆 + 三角旗）
fn icon_base() -> Vec<Vec<Vec2>> {
    vec![
        vec![Vec2::new(-0.1, -0.45), Vec2::new(-0.1, 0.45)],
        vec![Vec2::new(-0.1, 0.45), Vec2::new(0.45, 0.25), Vec2::new(-0.1, 0.05)],
    ]
}

/// 统一构造：frame 线稿 + icons 线稿 → 单 Mesh
fn milstd_build(frame_lines: &[Vec<Vec2>], icons: &[Vec<Vec2>]) -> Mesh {
    let frame_refs: Vec<&[Vec2]> = frame_lines.iter().map(|v| v.as_slice()).collect();
    let icon_refs: Vec<&[Vec2]> = icons.iter().map(|v| v.as_slice()).collect();
    let frame_mesh = polylines_mesh(&frame_refs, FRAME_W);
    let icon_mesh = polylines_mesh(&icon_refs, ICON_W);
    merge_meshes(frame_mesh, icon_mesh)
}

fn merge_meshes(a: Mesh, b: Mesh) -> Mesh {
    let (mut av, mut ai) = mesh_parts(a);
    let (bv, bi) = mesh_parts(b);
    let base = av.len() as u32;
    for i in bi {
        ai.push(base + i);
    }
    av.extend(bv);
    new_mesh(av, ai)
}

fn mesh_parts(m: Mesh) -> (Vec<[f32; 3]>, Vec<u32>) {
    let verts = m
        .attribute(Mesh::ATTRIBUTE_POSITION)
        .and_then(|v| v.as_float3()).map(|v| v.to_vec())
        .unwrap_or_default();
    let idx = m.indices().map(|i| match i {
        bevy::render::mesh::Indices::U32(v) => v.clone(),
        bevy::render::mesh::Indices::U16(v) => v.iter().map(|&x| x as u32).collect(),
    }).unwrap_or_default();
    (verts, idx)
}

/// 单位线段 (0,0)→(1,0)，宽 1，用 scale 控制长度/宽度
pub fn unit_seg_mesh() -> Mesh {
    let v: Vec<[f32; 3]> = [[0.0, -0.5, 0.0], [0.0, 0.5, 0.0], [1.0, 0.5, 0.0], [1.0, -0.5, 0.0]].to_vec();
    let idx = vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2];
    new_mesh(v, idx)
}

// ---------- 视觉资源 ----------

/// MIL-STD-2525 组合符号键：身份 × 领域 × 图标 × 分类状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MilSymbol {
    /// 友方：空中 / 水面 / 水下 / 地面设施
    FriendAir,
    FriendSea,
    FriendSub,
    FriendLand,   // 设施（雷达/基地共用地面矩形框）
    FriendRadar,  // 地面框 + 雷达图标
    FriendBase,   // 地面框 + 旗帜图标
    /// 敌方已分类：菱形 + 领域图标
    HostileAir,
    HostileSea,
    HostileSub,
    /// 敌方未分类：四叶形（无图标）
    Unknown,
    /// 中立：方菱形 + 船图标
    NeutralSea,
}

#[derive(Resource)]
pub struct UnitVisuals {
    pub mil: HashMap<MilSymbol, Handle<Mesh>>,
    pub annulus: Handle<Mesh>,
    pub unit_seg: Handle<Mesh>,
    pub mat_blue: Handle<ColorMaterial>,
    pub mat_red: Handle<ColorMaterial>,
    pub mat_neutral: Handle<ColorMaterial>,
    pub mat_yellow: Handle<ColorMaterial>,
    pub mat_select: Handle<ColorMaterial>,
    pub mat_route: Handle<ColorMaterial>,
    pub mat_sensor: Handle<ColorMaterial>,
}

pub fn init_unit_visuals(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    let mut add_mat = |c: Color| materials.add(ColorMaterial::from(c));
    // 敌方框架：尖顶菱形（复用现有 diamond 顶点，稍收窄）
    let hostile_frame = vec![vec![
        Vec2::new(0.0, 1.15),
        Vec2::new(0.85, 0.0),
        Vec2::new(0.0, -1.15),
        Vec2::new(-0.85, 0.0),
        Vec2::new(0.0, 1.15),
    ]];
    let mut mil = HashMap::new();
    let mut put = |k: MilSymbol, frame: Vec<Vec<Vec2>>, icons: Vec<Vec<Vec2>>| {
        mil.insert(k, meshes.add(milstd_build(&frame, &icons)));
    };
    put(MilSymbol::FriendAir, frame_air_lines(), icon_fixed_wing());
    put(MilSymbol::FriendSea, frame_sea_lines(), icon_ship());
    put(MilSymbol::FriendSub, frame_sub_lines(), icon_submarine());
    put(MilSymbol::FriendLand, frame_land_lines(), vec![]);
    put(MilSymbol::FriendRadar, frame_land_lines(), icon_radar());
    put(MilSymbol::FriendBase, frame_land_lines(), icon_base());
    put(MilSymbol::HostileAir, hostile_frame.clone(), icon_fixed_wing());
    put(MilSymbol::HostileSea, hostile_frame.clone(), icon_ship());
    put(MilSymbol::HostileSub, hostile_frame.clone(), icon_submarine());
    put(MilSymbol::Unknown, frame_quatrefoil_lines(), vec![]);
    put(MilSymbol::NeutralSea, frame_neutral_lines(), icon_ship());

    let v = UnitVisuals {
        mil,
        annulus: meshes.add(annulus_mesh(28)),
        unit_seg: meshes.add(unit_seg_mesh()),
        mat_blue: add_mat(palette::SIDE_BLUE),
        mat_red: add_mat(palette::SIDE_RED),
        mat_neutral: add_mat(palette::SIDE_NEUTRAL),
        mat_yellow: add_mat(palette::SELECT),
        mat_select: add_mat(palette::SELECT),
        mat_route: add_mat(palette::SELECT.with_alpha(0.55)),
        mat_sensor: add_mat(palette::SIDE_BLUE.with_alpha(0.20)),
    };
    // 选择环与传感器圈
    commands.spawn((
        Mesh2d(v.annulus.clone()),
        MeshMaterial2d(v.mat_select.clone()),
        Transform::from_xyz(0.0, 0.0, 10.4),
        Visibility::Hidden,
        SelectionRing,
    ));
    commands.spawn((
        Mesh2d(v.annulus.clone()),
        MeshMaterial2d(v.mat_sensor.clone()),
        Transform::from_xyz(0.0, 0.0, 9.5),
        Visibility::Hidden,
        SensorRing { kind: SensorRingKind::Radar },
    ));
    commands.spawn((
        Mesh2d(v.annulus.clone()),
        MeshMaterial2d(v.mat_sensor.clone()),
        Transform::from_xyz(0.0, 0.0, 9.5),
        Visibility::Hidden,
        SensorRing { kind: SensorRingKind::Sonar },
    ));
    // 航线段对象池
    for _ in 0..12 {
        commands.spawn((
            Mesh2d(v.unit_seg.clone()),
            MeshMaterial2d(v.mat_route.clone()),
            Transform::from_xyz(0.0, 0.0, 9.8),
            Visibility::Hidden,
            RouteLine,
        ));
    }
    commands.insert_resource(v);
}

// ---------- 标记组件 ----------

#[derive(Component)]
pub struct UnitSymbol {
    pub unit: Entity,
}
#[derive(Component)]
pub struct UnitLeader {
    pub unit: Entity,
}
#[derive(Component)]
pub struct UnitLabel {
    pub unit: Entity,
}
#[derive(Component)]
pub struct SelectionRing;
#[derive(Component)]
pub struct SensorRing {
    pub kind: SensorRingKind,
}
#[derive(Component)]
pub struct RouteLine;

#[derive(Clone, Copy, PartialEq)]
pub enum SensorRingKind {
    Radar,
    Sonar,
}

/// 新单位出现时生成符号/矢量线/标签
pub fn spawn_unit_visuals(
    new_units: Query<(Entity, &Unit), bevy::ecs::query::Added<Unit>>,
    visuals: Res<UnitVisuals>,
    mut commands: Commands,
) {
    for (e, u) in &new_units {
        let m = match u.side {
            Side::Blue => visuals.mat_blue.clone(),
            Side::Red => visuals.mat_red.clone(),
            Side::Neutral => visuals.mat_neutral.clone(),
        };
        let initial_mesh = visuals
            .mil
            .get(&mil_symbol_of(u))
            .cloned()
            .unwrap_or_else(|| visuals.mil.get(&MilSymbol::Unknown).cloned().unwrap());
        commands.spawn((
            Mesh2d(initial_mesh),
            MeshMaterial2d(m),
            Transform::from_xyz(0.0, 0.0, 10.0),
            Visibility::Visible,
            UnitSymbol { unit: e },
        ));
        commands.spawn((
            Mesh2d(visuals.unit_seg.clone()),
            MeshMaterial2d(visuals.mat_blue.clone()),
            Transform::from_xyz(0.0, 0.0, 10.05),
            Visibility::Hidden,
            UnitLeader { unit: e },
        ));
        commands.spawn((
            Text2d::new("UNIT"),
            TextColor(palette::SIDE_BLUE),
            Transform::from_xyz(0.0, 0.0, 10.3),
            Visibility::Visible,
            UnitLabel { unit: e },
        ));
    }
}

/// 单位 → MIL-STD-2525 组合键
pub fn mil_symbol_of(u: &Unit) -> MilSymbol {
    use crate::sim::PlatformKind as Pk;
    match u.side {
        Side::Red if !u.classified => MilSymbol::Unknown,
        Side::Red => match u.kind.domain() {
            Domain::Air => MilSymbol::HostileAir,
            Domain::Subsurface => MilSymbol::HostileSub,
            Domain::Surface => MilSymbol::HostileSea,
        },
        Side::Neutral => MilSymbol::NeutralSea,
        Side::Blue => match u.kind {
            Pk::Facility => {
                if u.hull == "RADAR" || u.name.contains("RADAR") {
                    MilSymbol::FriendRadar
                } else {
                    MilSymbol::FriendBase
                }
            }
            Pk::Submarine => MilSymbol::FriendSub,
            Pk::MPatrol | Pk::Fighter => MilSymbol::FriendAir,
            _ => MilSymbol::FriendSea,
        },
    }
}

/// fog of war + 阵营配色 + 2525 符号选择
pub fn symbol_style(u: &Unit, v: &UnitVisuals) -> (Handle<Mesh>, Handle<ColorMaterial>) {
    let mesh = v
        .mil
        .get(&mil_symbol_of(u))
        .cloned()
        .unwrap_or_else(|| v.mil.get(&MilSymbol::Unknown).cloned().unwrap());
    let mat = match u.side {
        Side::Red if !u.classified => v.mat_yellow.clone(),
        Side::Red => v.mat_red.clone(),
        Side::Neutral => v.mat_neutral.clone(),
        Side::Blue => v.mat_blue.clone(),
    };
    (mesh, mat)
}

pub fn sync_symbols(
    units: Query<(&Unit, &Position)>,
    visuals: Res<UnitVisuals>,
    rig: Res<CameraRig>,
    mut q: Query<(&UnitSymbol, &mut Transform, &mut Visibility, &mut Mesh2d, &mut MeshMaterial2d<ColorMaterial>)>,
) {
    let s = SYMBOL_PX * rig.mpp;
    for (sym, mut t, mut vis, mut mesh, mut mat) in &mut q {
        let Ok((u, pos)) = units.get(sym.unit) else {
            *vis = Visibility::Hidden;
            continue;
        };
        *vis = if u.side != Side::Red || u.detected_by_blue { Visibility::Visible } else { Visibility::Hidden };
        t.translation = pos.0.extend(10.0);
        t.scale = Vec3::new(s, s, 1.0);
        let (mh, mth) = symbol_style(u, &visuals);
        mesh.0 = mh;
        mat.0 = mth;
    }
}

/// 速度矢量线：1 分钟真实航程，屏幕恒定 1.6px 宽
pub fn sync_leaders(
    units: Query<(&Unit, &Position, &Heading, &SpeedMps)>,
    rig: Res<CameraRig>,
    mut q: Query<(&UnitLeader, &mut Transform, &mut Visibility, &mut MeshMaterial2d<ColorMaterial>)>,
    visuals: Res<UnitVisuals>,
) {
    for (ld, mut t, mut vis, mut mat) in &mut q {
        let Ok((u, pos, hdg, spd)) = units.get(ld.unit) else {
            *vis = Visibility::Hidden;
            continue;
        };
        let visible = spd.0 > 0.01 && (u.side != Side::Red || u.detected_by_blue);
        *vis = if visible { Visibility::Visible } else { Visibility::Hidden };
        if !visible {
            continue;
        }
        let len = spd.0 * 60.0; // 1 分钟航程（米）
        t.translation = pos.0.extend(10.05);
        t.rotation = bevy::math::Quat::from_rotation_z(hdg.0);
        t.scale = Vec3::new(len, 1.8 * rig.mpp, 1.0);
        let m = match u.side {
            Side::Red if !u.classified => visuals.mat_yellow.clone(),
            Side::Red => visuals.mat_red.clone(),
            Side::Neutral => visuals.mat_neutral.clone(),
            Side::Blue => visuals.mat_blue.clone(),
        };
        mat.0 = m;
    }
}

pub fn sync_labels(
    units: Query<(&Unit, &Position)>,
    rig: Res<CameraRig>,
    mut q: Query<(&UnitLabel, &mut Transform, &mut Visibility, &mut Text2d, &mut TextColor)>,
) {
    for (lb, mut t, mut vis, mut text, mut color) in &mut q {
        let Ok((u, pos)) = units.get(lb.unit) else {
            *vis = Visibility::Hidden;
            continue;
        };
        *vis = if u.side != Side::Red || u.detected_by_blue { Visibility::Visible } else { Visibility::Hidden };
        let unknown = u.side == Side::Red && !u.classified;
        let label = if unknown {
            "UNKNOWN".to_string()
        } else {
            format!("{} {}", u.kind.short(), u.name)
        };
        if text.0 != label {
            text.0 = label;
        }
        color.0 = match u.side {
            Side::Red if !u.classified => palette::SELECT,
            Side::Red => palette::SIDE_RED,
            Side::Neutral => palette::SIDE_NEUTRAL,
            Side::Blue => palette::SIDE_BLUE,
        };
        t.translation = (pos.0 + Vec2::new(0.0, -(SYMBOL_PX + 11.0) * rig.mpp)).extend(10.3);
        t.scale = Vec3::splat(rig.mpp);
    }
}

/// 选择环 + 传感器范围圈
pub fn sync_selection(
    units: Query<(&Unit, &Position)>,
    selection: Res<Selection>,
    rig: Res<CameraRig>,
    visuals: Res<UnitVisuals>,
    mut ring: Query<
        (&mut Transform, &mut Visibility),
        (With<SelectionRing>, Without<SensorRing>),
    >,
    mut sensors: Query<
        (&SensorRing, &mut Transform, &mut Visibility, &mut MeshMaterial2d<ColorMaterial>),
        Without<SelectionRing>,
    >,
) {
    let Some(sel) = selection.0 else {
        if let Ok((_, mut vis)) = ring.single_mut() {
            *vis = Visibility::Hidden;
        }
        for (_, _, mut vis, _) in &mut sensors {
            *vis = Visibility::Hidden;
        }
        return;
    };
    let Ok((u, pos)) = units.get(sel) else { return };
    if let Ok((mut t, mut vis)) = ring.single_mut() {
        let s = (SYMBOL_PX + 6.0) * rig.mpp;
        t.translation = pos.0.extend(10.4);
        t.scale = Vec3::new(s, s, 1.0);
        *vis = Visibility::Visible;
    }
    for (sr, mut t, mut vis, mut mat) in &mut sensors {
        let range = match sr.kind {
            SensorRingKind::Radar => u.sensors.radar_m,
            SensorRingKind::Sonar => u.sensors.sonar_m,
        };
        match range {
            Some(r) => {
                *vis = if u.side == Side::Blue { Visibility::Visible } else { Visibility::Hidden };
                t.translation = pos.0.extend(9.5);
                t.scale = Vec3::new(r, r, 1.0);
                mat.0 = visuals.mat_sensor.clone();
            }
            None => *vis = Visibility::Hidden,
        }
    }
}

/// 航线：当前航段 + 后续航段（循环航线闭合）
pub fn sync_routes(
    units: Query<(&Unit, &Position)>,
    selection: Res<Selection>,
    rig: Res<CameraRig>,
    mut pool: Query<&mut Transform, With<RouteLine>>,
) {
    let Some(sel) = selection.0 else {
        for mut t in &mut pool {
            t.scale = Vec3::ZERO;
        }
        return;
    };
    let Ok((u, pos)) = units.get(sel) else { return };
    if u.side != Side::Blue || u.route.is_empty() {
        for mut t in &mut pool {
            t.scale = Vec3::ZERO;
        }
        return;
    }
    let width = 2.0 * rig.mpp;
    let mut legs: Vec<(Vec2, Vec2)> = Vec::new();
    if u.moving {
        let start = pos.0;
        let n = u.route.len();
        legs.push((start, u.route[u.wp_index]));
        for i in u.wp_index..n.saturating_sub(1) {
            legs.push((u.route[i], u.route[i + 1]));
        }
        if u.route_loop && n >= 2 {
            legs.push((u.route[n - 1], u.route[0]));
        }
    }
    for (leg, mut t) in legs.iter().zip(pool.iter_mut()) {
        let (a, b) = *leg;
        let d = b - a;
        let len = d.length();
        if len < 1.0 {
            t.scale = Vec3::ZERO;
            continue;
        }
        t.translation = a.extend(9.8);
        t.rotation = bevy::math::Quat::from_rotation_z(d.y.atan2(d.x));
        t.scale = Vec3::new(len, width, 1.0);
    }
    // 隐藏对象池中剩余线段
    for mut t in pool.iter_mut().skip(legs.len()) {
        t.scale = Vec3::ZERO;
    }
}
#[cfg(test)]
mod milstd_tests {
    use super::*;

    fn mesh_pos(m: &Mesh) -> Vec<[f32; 3]> {
        use bevy::render::mesh::VertexAttributeValues;
        m.attribute(Mesh::ATTRIBUTE_POSITION)
            .and_then(|v| v.as_float3())
            .map(|v| v.to_vec())
            .unwrap_or_default()
    }

    #[test]
    fn frames_have_vertices() {
        for (name, frame) in [
            ("land", frame_land_lines()),
            ("air", frame_air_lines()),
            ("sea", frame_sea_lines()),
            ("sub", frame_sub_lines()),
            ("neutral", frame_neutral_lines()),
            ("quatrefoil", frame_quatrefoil_lines()),
        ] {
            assert!(!frame.is_empty(), "{name} 框架线稿为空");
            let m = milstd_build(&frame, &[]);
            let pts = mesh_pos(&m);
            assert!(pts.len() >= 5, "{name} 顶点过少: {}", pts.len());
            assert!(m.indices().is_some(), "{name} 无索引");
        }
    }

    #[test]
    fn frame_with_icon_merges() {
        let m = milstd_build(&frame_sea_lines(), &icon_ship());
        let frame_only = milstd_build(&frame_sea_lines(), &[]);
        let icon_only = milstd_build(&[], &icon_ship());
        let total = mesh_pos(&frame_only).len() + mesh_pos(&icon_only).len();
        assert_eq!(mesh_pos(&m).len(), total, "合并网格顶点数应等于两部分之和");
    }

    #[test]
    fn hostile_frame_is_diamond() {
        let m = milstd_build(&[vec![
            Vec2::new(0.0, 1.15), Vec2::new(0.85, 0.0),
            Vec2::new(0.0, -1.15), Vec2::new(-0.85, 0.0), Vec2::new(0.0, 1.15),
        ]], &[]);
        let pts = mesh_pos(&m);
        let ys: Vec<f32> = pts.iter().map(|v| v[1]).collect();
                // seg_quad 端点外延半宽（接缝设计），极值允许 ±半宽
        assert!((ys.iter().cloned().fold(f32::MIN, f32::max) - 1.15).abs() < 0.15);
        assert!((ys.iter().cloned().fold(f32::MAX, f32::min) + 1.15).abs() < 0.15);
    }
}
