//! 地图图层网格构建与场景生成。
//!
//! 所有静态地图要素合并为少量大 Mesh（每层一个），用 Mesh2d + ColorMaterial 渲染。
//! 面要素经 earcut 三角化；线要素展开为四边形（端点外延半个宽度作接缝）。

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use earcut::Earcut;

use crate::geo::Projection;
use crate::osm::{Line, LineKind, MapData, Poly, PolyKind};

/// 暗色战术主题配色（贴近 CMO 夜间海图观感）
pub mod palette {
    use bevy::color::Color;

    pub const LAND_BG: Color = Color::srgb_u8(21, 25, 30);
    pub const WATER: Color = Color::srgb_u8(15, 47, 68);
    pub const GREEN: Color = Color::srgb_u8(22, 35, 27);
    pub const LANDUSE: Color = Color::srgb_u8(27, 33, 41);
    pub const BUILDING: Color = Color::srgb_u8(37, 43, 52);
    pub const APRON: Color = Color::srgb_u8(42, 47, 54);
    pub const ROAD_MAJOR: Color = Color::srgb_u8(72, 82, 95);
    pub const ROAD_MID: Color = Color::srgb_u8(56, 65, 76);
    pub const ROAD_MINOR: Color = Color::srgb_u8(44, 52, 62);
    pub const RAIL: Color = Color::srgb_u8(58, 50, 66);
    pub const RUNWAY: Color = Color::srgb_u8(64, 70, 78);
    pub const TAXIWAY: Color = Color::srgb_u8(49, 54, 60);
    pub const GRATICULE: Color = Color::srgba_u8(255, 255, 255, 16);

    pub const SIDE_BLUE: Color = Color::srgb_u8(97, 165, 255);
    pub const SIDE_RED: Color = Color::srgb_u8(255, 95, 86);
    pub const SIDE_NEUTRAL: Color = Color::srgb_u8(82, 201, 135);
    pub const SELECT: Color = Color::srgb_u8(255, 224, 109);
}

fn new_mesh(positions: Vec<[f32; 3]>, indices: Vec<u32>) -> Mesh {
    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

/// 顶点色合并网格的累积器：每图层按序追加（后追加的覆盖先追加的，替代多实体 z 排序）
#[derive(Default)]
pub struct MeshAccumulator {
    positions: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    indices: Vec<u32>,
}

impl MeshAccumulator {
    /// 追加一个已构建图层的网格（顶点色统一为该层颜色，linear 空间）
    pub fn append_mesh(&mut self, mesh: &Mesh, color: Color) {
        let verts = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .and_then(|v| v.as_float3())
            .map(|v| v.to_vec())
            .unwrap_or_default();
        let idx = mesh
            .indices()
            .map(|i| match i {
                Indices::U32(v) => v.clone(),
                Indices::U16(v) => v.iter().map(|&x| x as u32).collect(),
            })
            .unwrap_or_default();
        let base = self.positions.len() as u32;
        let rgba = color.to_linear().to_f32_array();
        self.positions.extend(verts);
        self.colors.extend(std::iter::repeat(rgba).take(mesh.attribute(Mesh::ATTRIBUTE_POSITION).map(|v| v.len()).unwrap_or(0)));
        self.indices.extend(idx.iter().map(|&i| base + i));
    }

    /// 生成带顶点色的单 Mesh（ColorMaterial 白色 × 顶点色）
    pub fn build(self) -> Mesh {
        let mut mesh = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, self.positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, self.colors);
        mesh.insert_indices(Indices::U32(self.indices));
        mesh
    }
}

/// 面要素集合 → 单个三角化 Mesh（含洞）
pub fn poly_mesh(polys: &[Poly]) -> Mesh {
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    let mut ear = Earcut::new();
    for poly in polys {
        let base = verts.len() as u32;
        let mut data: Vec<[f64; 2]> = Vec::with_capacity(poly.outer.len());
        for p in &poly.outer {
            data.push([p.x as f64, p.y as f64]);
        }
        let mut hole_indices: Vec<usize> = Vec::new();
        for hole in &poly.holes {
            hole_indices.push(data.len());
            for p in hole {
                data.push([p.x as f64, p.y as f64]);
            }
        }
        let mut tris: Vec<usize> = Vec::new();
        ear.earcut(data.iter().copied(), &hole_indices, &mut tris);
        if tris.len() < 3 {
            continue; // 退化多边形
        }
        for [x, y] in &data {
            verts.push([*x as f32, *y as f32, 0.0]);
        }
        for t in &tris {
            idx.push(base + *t as u32);
        }
    }
    new_mesh(verts, idx)
}

/// 线要素集合 → 四边形 Mesh。双面输出三角形，避免绕序剔除问题。
pub fn line_mesh(lines: &[Line]) -> Mesh {
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut idx: Vec<u32> = Vec::new();
    for line in lines {
        polyline_strip(&line.pts, line.width, false, &mut verts, &mut idx);
    }
    new_mesh(verts, idx)
}

/// 折线 → miter-join 条带。
///
/// 相邻段在共享点用 miter 点（法线平均 + 半角扩展）连接：
/// - 内部顶点左右成对（2N 顶点、6(N-1) 索引），替代逐段独立四边形的 4(N-1) 顶点 / 12(N-1) 索引；
/// - 消除斜接头处的外角缺口与内角自重叠；
/// - `closed` 时首尾焊接（索引回绕首点对），闭合框无接缝。
/// 端点（非闭合）外延半宽作 square cap，掩盖道路交叉口的 way 接缝。
pub(crate) fn polyline_strip(
    pts: &[Vec2],
    width: f32,
    closed: bool,
    verts: &mut Vec<[f32; 3]>,
    idx: &mut Vec<u32>,
) {
    let n = pts.len();
    if n < 2 {
        return;
    }
    let half = width * 0.5;
    let miter_limit = 2.0 * half; // miter 长度上限（超过退化为平头）

    // 每个折线点的“扩展法线”（miter 向量 × half）
    let mut offsets: Vec<Vec2> = Vec::with_capacity(n);
    for i in 0..n {
        let prev = if i == 0 {
            if closed { pts[n - 2] } else { pts[0] }
        } else {
            pts[i - 1]
        };
        let next = if i == n - 1 {
            if closed { pts[1] } else { pts[n - 1] }
        } else {
            pts[i + 1]
        };
        let d1 = pts[i] - prev;
        let d2 = next - pts[i];
        let l1 = d1.length();
        let l2 = d2.length();
        let (n1, n2) = if l1 < 1e-6 || l2 < 1e-6 {
            let d = if l2 >= l1 { d2 } else { d1 };
            let dl = d.length();
            if dl < 1e-6 {
                offsets.push(Vec2::ZERO);
                continue;
            }
            let nn = Vec2::new(-d.y, d.x) / dl;
            (nn, nn)
        } else {
            (Vec2::new(-d1.y, d1.x) / l1, Vec2::new(-d2.y, d2.x) / l2)
        };
        // miter：法线平均归一化，按 cos(半角) 扩展
        let m = n1 + n2;
        let ml = m.length();
        let offset = if ml < 1e-6 {
            // 180° 折返：用 n1（任意一侧）
            n1 * half
        } else {
            let m = m / ml;
            let cos_half = (n1.dot(m)).max(0.2); // clamp 防爆炸（对应 miter limit ≈ 5）
            let len = (half / cos_half).min(miter_limit);
            m * len
        };
        offsets.push(offset);
    }

    let base = verts.len() as u32;
    for i in 0..n {
        let o = offsets[i];
        let mut p = pts[i];
        if !closed {
            if i == 0 {
                p -= (pts[1] - pts[0]).normalize_or_zero() * half;
            } else if i == n - 1 {
                p += (pts[n - 1] - pts[n - 2]).normalize_or_zero() * half;
            }
        }
        verts.push([(p - o).x, (p - o).y, 0.0]);
        verts.push([(p + o).x, (p + o).y, 0.0]);
    }
    for i in 0..n - 1 {
        let l = base + i as u32 * 2;
        // 左右成对：L(i) R(i) R(i+1) L(i+1)
        idx.extend_from_slice(&[l, l + 1, l + 3, l, l + 3, l + 2]);
    }
    if closed && n >= 3 {
        // 闭合焊接：末点 (n-1) 连回首点 0（首点 miter 已含末段方向）
        let first = base;
        let last = base + (n - 1) as u32 * 2;
        idx.extend_from_slice(&[last, last + 1, first + 1, last, first + 1, first]);
    }
}


/// 经纬网线集（区域由经纬边界给定；等纬线/经线在 Web Mercator 下均为直线）
pub fn graticule_lines_region(
    proj: &Projection,
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
    step_deg: f64,
    width_m: f32,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let w = width_m;
    let mut lat = (min_lat / step_deg).ceil() * step_deg;
    while lat <= max_lat {
        let a = proj.project(lat, min_lon);
        let b = proj.project(lat, max_lon);
        lines.push(Line { kind: LineKind::Rail, pts: vec![a, b], width: w });
        lat += step_deg;
    }
    let mut lon = (min_lon / step_deg).ceil() * step_deg;
    while lon <= max_lon {
        let a = proj.project(min_lat, lon);
        let b = proj.project(max_lat, lon);
        lines.push(Line { kind: LineKind::Rail, pts: vec![a, b], width: w });
        lon += step_deg;
    }
    lines
}

/// 独立经纬网层：随真实视距重建（瓦片 mesh 内嵌的网格步长固定，无法随缩放自适应）
#[derive(Component)]
pub struct GraticuleLayer;

#[derive(Resource, Default)]
pub struct GraticuleState {
    step: f64,
    center: Vec2,
}

pub fn graticule_system(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut state: ResMut<GraticuleState>,
    rig: Res<crate::camera::CameraRig>,
    ctx: Res<crate::MapCtx>,
    win: Query<&bevy::window::Window, With<bevy::window::PrimaryWindow>>,
    old: Query<Entity, With<GraticuleLayer>>,
) {
    let (w, h) = win.single().map(|w| (w.width(), w.height())).unwrap_or((1600.0, 900.0));
    let view_h_m = rig.mpp * h;
    let view_w_m = rig.mpp * w;
    let step = graticule_step_for_view(view_h_m, h);
    // 步长变化或中心移出半屏（缓冲覆盖 1.5 屏）才重建
    let need = GraticuleState { step, center: rig.target }.step != state.step
        || state.center.distance(rig.target) > view_h_m.max(view_w_m) * 0.5;
    if !need {
        return;
    }
    for e in &old {
        commands.entity(e).despawn();
    }
    let half = Vec2::new(view_w_m, view_h_m) * 0.75;
    let (lat_s, lon_w) = ctx.proj.unproject(rig.target - half);
    let (lat_n, lon_e) = ctx.proj.unproject(rig.target + half);
    let lines = graticule_lines_region(&ctx.proj, lat_s, lon_w, lat_n, lon_e, step, rig.mpp * 1.2);
    if lines.is_empty() {
        *state = GraticuleState { step, center: rig.target };
        return;
    }
    let mesh = meshes.add(line_mesh(&lines));
    commands.spawn((
        bevy::render::mesh::Mesh2d(mesh),
        MeshMaterial2d(materials.add(ColorMaterial::from(palette::GRATICULE))),
        // 网格画在瓦片层（z=0 平面）之上
        Transform::from_xyz(0.0, 0.0, 0.5),
        Visibility::Visible,
        GraticuleLayer,
    ));
    *state = GraticuleState { step, center: rig.target };
}

/// 经纬网步长：按视距自适应（目标屏幕间距 ~110px），
/// 度数取整到标准系列——避免低视距时 0.05° 固定网格密集成一片（Cesium 默认甚至不显示经纬网）。
/// `bounds_m` 为视口高（米），`bounds_h_px` 为视口像素高。
pub fn graticule_step_for_view(bounds_m: f32, bounds_h_px: f32) -> f64 {
    const STEPS: [f64; 9] = [0.01, 0.02, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0];
    // 屏幕间距 110px 对应的米数 → 近似度数（经度方向）
    let want_deg = (bounds_m / bounds_h_px.max(1.0) * 110.0) as f64 / 111_320.0;
    STEPS.iter().copied().find(|&s| s >= want_deg).unwrap_or(5.0)
}

/// 场景中的一层：颜色 + z 序 + 网格（可在任务线程构建，主线程生成实体）
pub struct MapLayer {
    pub color: Color,
    #[allow(dead_code)] // 顶点色合并后层级由追加序决定
    pub z: f32,
    pub mesh: Mesh,
}

/// 构建全部地图层并合并为单个顶点色网格（纯函数，可在任务线程调用）。
/// 层序即绘制序（后追加覆盖先追加），替代多实体的 z 排序。
pub fn build_map_mesh(map: &MapData) -> Mesh {
    let mut acc = MeshAccumulator::default();
    let mut layers: Vec<MapLayer> = Vec::new();

    let push_polys = |kind: PolyKind, color: Color, z: f32, layers: &mut Vec<MapLayer>| {
        let polys: Vec<&Poly> = map.polys.iter().filter(|p| p.kind == kind).collect();
        if polys.is_empty() {
            return;
        }
        let owned: Vec<Poly> = polys.into_iter().cloned().collect();
        layers.push(MapLayer { color, z, mesh: poly_mesh(&owned) });
    };
    push_polys(PolyKind::Water, palette::WATER, 1.0, &mut layers);
    push_polys(PolyKind::Landuse, palette::LANDUSE, 2.0, &mut layers);
    push_polys(PolyKind::Green, palette::GREEN, 3.0, &mut layers);
    push_polys(PolyKind::Apron, palette::APRON, 3.5, &mut layers);
    push_polys(PolyKind::Building, palette::BUILDING, 4.0, &mut layers);

    let push_lines = |kind_pred: fn(&LineKind) -> bool, color: Color, z: f32, layers: &mut Vec<MapLayer>| {
        let lines: Vec<Line> = map.lines.iter().filter(|l| kind_pred(&l.kind)).cloned().collect();
        if lines.is_empty() {
            return;
        }
        layers.push(MapLayer { color, z, mesh: line_mesh(&lines) });
    };
    push_lines(|k| *k == LineKind::Waterway, palette::WATER, 4.5, &mut layers);
    push_lines(|k| *k == LineKind::Rail, palette::RAIL, 5.0, &mut layers);
    push_lines(|k| *k == LineKind::Taxiway, palette::TAXIWAY, 5.1, &mut layers);
    push_lines(|k| *k == LineKind::Runway, palette::RUNWAY, 5.2, &mut layers);
    push_lines(|k| *k == LineKind::Road(crate::osm::RoadClass::Minor), palette::ROAD_MINOR, 5.3, &mut layers);
    push_lines(|k| *k == LineKind::Road(crate::osm::RoadClass::Mid), palette::ROAD_MID, 5.4, &mut layers);
    push_lines(|k| *k == LineKind::Road(crate::osm::RoadClass::Major), palette::ROAD_MAJOR, 5.5, &mut layers);

    for layer in &layers {
        acc.append_mesh(&layer.mesh, layer.color);
    }
    acc.build()
}

/// 已生成的地图网格实体（供卸载时回收资产）
pub struct SpawnedMapLayer {
    pub entity: Entity,
    pub mesh: Handle<Mesh>,
    #[allow(dead_code)] // 共享材质不回收，保留供调试
    pub material: Handle<ColorMaterial>,
}

/// 把合并网格生成单实体（主线程）。`origin` 为网格局部坐标原点的世界位置。
/// 白色 Blend 材质 × 顶点色；全局材质由 `white_vertex_material` 提供。
pub fn spawn_map_layers_at(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    _materials: &mut Assets<ColorMaterial>,
    mesh: Mesh,
    origin: Vec2,
    shared_material: &Handle<ColorMaterial>,
) -> SpawnedMapLayer {
    let handle = meshes.add(mesh);
    let entity = commands
        .spawn((
            Mesh2d(handle.clone()),
            MeshMaterial2d(shared_material.clone()),
            Transform::from_xyz(origin.x, origin.y, 0.0),
        ))
        .id();
    SpawnedMapLayer { entity, mesh: handle, material: shared_material.clone() }
}

/// 顶点色渲染共享材质（白色 × Blend，保证图层间按顶点序混合）
pub fn white_vertex_material(materials: &mut Assets<ColorMaterial>) -> Handle<ColorMaterial> {
    // ColorMaterial 默认 AlphaMode2d::Blend（顶点序即混合序）
    materials.add(ColorMaterial { color: Color::WHITE, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osm::RoadClass;

    fn line(pts: Vec<Vec2>) -> Line {
        Line { kind: LineKind::Road(RoadClass::Major), pts, width: 10.0 }
    }

    #[test]
    fn poly_mesh_square() {
        let square = Poly {
            kind: PolyKind::Building,
            outer: vec![
                Vec2::new(0.0, 0.0),
                Vec2::new(100.0, 0.0),
                Vec2::new(100.0, 100.0),
                Vec2::new(0.0, 100.0),
            ],
            holes: vec![],
        };
        let mesh = poly_mesh(&[square]);
        let pos = mesh.attribute(Mesh::ATTRIBUTE_POSITION).expect("应有位置属性");
        assert_eq!(pos.len(), 4, "正方形应有 4 顶点");
        let idx_count = mesh.indices().expect("应有索引").len();
        assert_eq!(idx_count, 6, "正方形应三角化为 2 个三角形，实际索引 {idx_count}");
    }

    #[test]
    fn poly_mesh_with_hole() {
        let outer = vec![
            Vec2::new(0.0, 0.0),
            Vec2::new(1000.0, 0.0),
            Vec2::new(1000.0, 1000.0),
            Vec2::new(0.0, 1000.0),
        ];
        let hole = vec![
            Vec2::new(400.0, 400.0),
            Vec2::new(600.0, 400.0),
            Vec2::new(600.0, 600.0),
            Vec2::new(400.0, 600.0),
        ];
        let poly = Poly { kind: PolyKind::Water, outer, holes: vec![hole] };
        let mesh = poly_mesh(&[poly]);
        let idx = mesh.indices().expect("应有索引");
        // 带洞矩形三角化：8 顶点，索引数应为 3 的倍数且 > 6
        assert_eq!(mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().len(), 8);
        assert!(idx.len() % 3 == 0 && idx.len() > 6, "带洞多边形索引数异常: {}", idx.len());
    }

    #[test]
    fn line_mesh_strip_structure() {
        let l = line(vec![Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0), Vec2::new(100.0, 100.0)]);
        let mesh = line_mesh(&[l]);
        // miter strip：3 点折线 → 每点 2 顶点
        assert_eq!(mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().len(), 6);
        // 2 段 × 单面 2 三角形 × 3 索引
        assert_eq!(mesh.indices().unwrap().len(), 12);
    }

    #[test]
    fn polyline_strip_closed_welds() {
        let mut verts: Vec<[f32; 3]> = Vec::new();
        let mut idx: Vec<u32> = Vec::new();
        // 正方形闭合框（不重复首点）
        polyline_strip(
            &[Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0), Vec2::new(10.0, 10.0), Vec2::new(0.0, 10.0)],
            2.0,
            true,
            &mut verts,
            &mut idx,
        );
        // 4 点 × 2 顶点
        assert_eq!(verts.len(), 8);
        // 4 段（含焊接末段）× 6 索引
        assert_eq!(idx.len(), 24);
        // 直角处 miter 扩展：角点偏移应大于 half（1.0）——正方形直角 cos45°≈0.707 → 偏移 ≈1.414
        // 检查第 2 点（直角）的左右顶点间距 ≈ 2×1.414
        let l = Vec2::new(verts[2][0], verts[2][1]);
        let r = Vec2::new(verts[3][0], verts[3][1]);
        let d = (r - l).length();
        assert!((d - 2.0 * 2f32.sqrt()).abs() < 0.05, "直角 miter 间距 = {d}");
    }

    #[test]
    fn polyline_strip_open_caps() {
        let mut verts: Vec<[f32; 3]> = Vec::new();
        let mut idx: Vec<u32> = Vec::new();
        polyline_strip(&[Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0)], 10.0, false, &mut verts, &mut idx);
        // 单段：4 顶点、单面 6 索引
        assert_eq!(verts.len(), 4);
        assert_eq!(idx.len(), 6);
        // square cap 外延 5：首点 x = -5，末点 x = 105
        assert!((verts[0][0] + 5.0).abs() < 1e-3);
        assert!((verts[2][0] - 105.0).abs() < 1e-3);
    }

    #[test]
    fn pearl_harbor_line_stats() {
        let xml = std::fs::read_to_string("data/pearl_harbor.osm").unwrap();
        let data = crate::osm::parse_osm(&xml).unwrap();
        let proj = data.projection().unwrap();
        let map = crate::osm::extract_map(&data, &proj);
        let mesh = line_mesh(&map.lines);
        let v = mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().len();
        let i = mesh.indices().unwrap().len();
        let segs: usize = map.lines.iter().map(|l| l.pts.len() - 1).sum();
        println!("珍珠港线层: {} 条线 / {} 段 | strip v={v} i={i} | legacy v={} i={}",
            map.lines.len(), segs, segs * 4, segs * 12);
    }

    #[test]
    fn road_layer_mesh_stats() {
        // 模拟 5000 条 8 点道路：对比 strip vs 逐段四边形的顶点/索引量
        let roads: Vec<Line> = (0..5000)
            .map(|k| Line {
                kind: LineKind::Road(RoadClass::Minor),
                pts: (0..8).map(|i| Vec2::new(i as f32 * 10.0, (i % 3) as f32 * 7.0 + k as f32)).collect(),
                width: 10.0,
            })
            .collect();
        let mesh = line_mesh(&roads);
        let v = mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().len();
        let i = mesh.indices().unwrap().len();
        let segs: u32 = roads.iter().map(|l| l.pts.len() as u32 - 1).sum();
        let old_v = (segs * 4) as usize;
        let old_i = (segs * 12) as usize;
        println!("strip: v={v} i={i} | legacy: v={old_v} i={old_i} | 顶点 {}% 索引 {}%",
            v * 100 / old_v, i * 100 / old_i);
        assert!(v < old_v && i < old_i);
    }

    #[test]
    fn graticule_step_adapts_to_view() {
        // 高视距（粗略全球）：步长大
        assert_eq!(graticule_step_for_view(40_000_000.0, 900.0), 5.0);
        // 战术视距（~220km 视高）：0.25°（屏幕间距 ~100px，不再 0.05° 密集网格）
        assert_eq!(graticule_step_for_view(223_403.0, 900.0), 0.25);
        // 近距：0.05°
        assert_eq!(graticule_step_for_view(30_000.0, 900.0), 0.05);
        // 更近：0.02/0.01
        assert_eq!(graticule_step_for_view(8_000.0, 900.0), 0.01);
    }

    #[test]
    fn graticule_produces_lines() {
        let proj = Projection::new(21.35, -157.92);
        let lines = graticule_lines_region(&proj, 21.30, -157.98, 21.40, -157.87, 0.05, 15.0);
        assert!(!lines.is_empty(), "应生成经纬网线");
        assert!(lines.iter().all(|l| l.pts.len() == 2));
    }
}
