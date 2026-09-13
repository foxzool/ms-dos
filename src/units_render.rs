//! 单位可视化：NTDS 风格符号、速度矢量线、标签、选择环、传感器范围圈、航线。
//!
//! 符号网格以“半径 1”建模，运行时按 `mpp` 缩放保持屏幕像素恒定；
//! 速度矢量线是真实物理长度（1 分钟航程）。

use bevy::asset::RenderAssetUsages;
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

/// 半圆（上/下半球，半径 1）
pub fn dome_mesh(up: bool, segs: usize) -> Mesh {
    let mut verts = vec![[0.0, 0.0, 0.0]];
    let mut idx = Vec::new();
    let sign = if up { 1.0 } else { -1.0 };
    for i in 0..=segs {
        let a = std::f32::consts::PI * (i as f32) / segs as f32;
        let p = Vec2::new(a.cos(), a.sin() * sign);
        verts.push([p.x, p.y, 0.0]);
    }
    for i in 1..=segs {
        idx.extend_from_slice(&[0u32, i as u32, (i + 1) as u32]);
        idx.extend_from_slice(&[0u32, (i + 1) as u32, i as u32]);
    }
    new_mesh(verts, idx)
}

/// 空心方形（设施），边框四段
pub fn square_outline_mesh() -> Mesh {
    let h = 0.85f32;
    let t = 0.30f32;
    let corners = [
        (Vec2::new(-h, h), Vec2::new(h, h)),
        (Vec2::new(h, h), Vec2::new(h, -h)),
        (Vec2::new(h, -h), Vec2::new(-h, -h)),
        (Vec2::new(-h, -h), Vec2::new(-h, h)),
    ];
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    for (a, b) in corners {
        seg_quad(&a, &b, t, &mut verts, &mut idx);
    }
    new_mesh(verts, idx)
}

/// 实心菱形（敌/未知）
pub fn diamond_mesh() -> Mesh {
    let v: Vec<[f32; 3]> = [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [-1.0, 0.0, 0.0]].to_vec();
    let idx = vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2];
    new_mesh(v, idx)
}

/// 实心小圆（中立）
pub fn dot_mesh(segs: usize) -> Mesh {
    let mut verts = vec![[0.0, 0.0, 0.0]];
    let mut idx = Vec::new();
    for i in 0..=segs {
        let a = (i as f32) * std::f32::consts::TAU / segs as f32;
        verts.push([a.cos() * 0.7, a.sin() * 0.7, 0.0]);
    }
    for i in 1..=segs {
        idx.extend_from_slice(&[0u32, i as u32, (i + 1) as u32]);
        idx.extend_from_slice(&[0u32, (i + 1) as u32, i as u32]);
    }
    new_mesh(verts, idx)
}

/// 单位线段 (0,0)→(1,0)，宽 1，用 scale 控制长度/宽度
pub fn unit_seg_mesh() -> Mesh {
    let v: Vec<[f32; 3]> = [[0.0, -0.5, 0.0], [0.0, 0.5, 0.0], [1.0, 0.5, 0.0], [1.0, -0.5, 0.0]].to_vec();
    let idx = vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2];
    new_mesh(v, idx)
}

// ---------- 视觉资源 ----------

#[derive(Resource)]
pub struct UnitVisuals {
    pub annulus: Handle<Mesh>,
    pub dome_up: Handle<Mesh>,
    pub dome_down: Handle<Mesh>,
    pub square: Handle<Mesh>,
    pub diamond: Handle<Mesh>,
    pub dot: Handle<Mesh>,
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
    let v = UnitVisuals {
        annulus: meshes.add(annulus_mesh(28)),
        dome_up: meshes.add(dome_mesh(true, 20)),
        dome_down: meshes.add(dome_mesh(false, 20)),
        square: meshes.add(square_outline_mesh()),
        diamond: meshes.add(diamond_mesh()),
        dot: meshes.add(dot_mesh(16)),
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
        commands.spawn((
            Mesh2d(visuals.annulus.clone()),
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

/// fog of war + 阵营配色 + 符号形状选择
pub fn symbol_style(u: &Unit, v: &UnitVisuals) -> (Handle<Mesh>, Handle<ColorMaterial>) {
    match u.side {
        Side::Red => {
            if u.classified {
                (v.diamond.clone(), v.mat_red.clone())
            } else {
                (v.diamond.clone(), v.mat_yellow.clone())
            }
        }
        Side::Neutral => (v.dot.clone(), v.mat_neutral.clone()),
        Side::Blue => {
            let mesh = match u.kind.domain() {
                Domain::Air => v.dome_up.clone(),
                Domain::Subsurface => v.dome_down.clone(),
                Domain::Surface => {
                    if u.kind == crate::sim::PlatformKind::Facility {
                        v.square.clone()
                    } else {
                        v.annulus.clone()
                    }
                }
            };
            (mesh, v.mat_blue.clone())
        }
    }
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
