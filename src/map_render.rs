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
        for pair in line.pts.windows(2) {
            let a = pair[0];
            let b = pair[1];
            let n = seg_quad(&a, &b, line.width, &mut verts, &mut idx);
            let _ = n;
        }
    }
    new_mesh(verts, idx)
}

/// 单段线 → 四边形，返回起始顶点索引
pub(crate) fn seg_quad(
    a: &Vec2,
    b: &Vec2,
    width: f32,
    verts: &mut Vec<[f32; 3]>,
    idx: &mut Vec<u32>,
) -> u32 {
    let base = verts.len() as u32;
    let dir = (*b - *a).normalize_or_zero();
    if dir.length_squared() < 1e-8 {
        return base;
    }
    let n = Vec2::new(-dir.y, dir.x) * (width * 0.5);
    // 端点外延半宽，避免折线接缝出现缺口
    let a = *a - dir * width * 0.5;
    let b = *b + dir * width * 0.5;
    verts.push([(a - n).x, (a - n).y, 0.0]);
    verts.push([(a + n).x, (a + n).y, 0.0]);
    verts.push([(b + n).x, (b + n).y, 0.0]);
    verts.push([(b - n).x, (b - n).y, 0.0]);
    // 双面三角形
    idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    idx.extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
    base
}

/// 经纬网（默认 0.05 度间隔，跨数据外接框外扩 20%）
pub fn graticule_lines(map: &MapData, proj: &Projection, step_deg: f64) -> Vec<Line> {
    let (mut min_lat, mut max_lat) = proj.unproject(map.min);
    let (max_lat2, min_lat2) = proj.unproject(map.max);
    min_lat = min_lat.min(min_lat2);
    max_lat = max_lat.max(max_lat2);
    let (mut min_lon, mut max_lon) = proj.unproject(Vec2::new(map.min.x, map.max.y));
    let (lon2_max, lon2_min) = proj.unproject(Vec2::new(map.max.x, map.min.y));
    min_lon = min_lon.min(lon2_min);
    max_lon = max_lon.max(lon2_max);

    let pad_lat = (max_lat - min_lat) * 0.2;
    let pad_lon = (max_lon - min_lon) * 0.2;
    min_lat -= pad_lat;
    max_lat += pad_lat;
    min_lon -= pad_lon;
    max_lon += pad_lon;

    let mut lines = Vec::new();
    let w = 40.0;
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

/// 场景中的一层：颜色 + z 序 + 网格（可在任务线程构建，主线程生成实体）
pub struct MapLayer {
    pub color: Color,
    pub z: f32,
    pub mesh: Mesh,
}

/// 构建全部地图层（纯函数，可在任务线程调用）。
/// `proj` 应为以网格局部原点为中心的投影，返回顶点为局部坐标。
pub fn build_map_layers(map: &MapData, proj: &Projection) -> Vec<MapLayer> {
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

    let grat = graticule_lines(map, proj, 0.05);
    layers.push(MapLayer { color: palette::GRATICULE, z: 8.0, mesh: line_mesh(&grat) });

    layers
}

/// 已生成的地图层实体（供卸载时回收资产）
pub struct SpawnedMapLayer {
    pub entity: Entity,
    pub mesh: Handle<Mesh>,
    pub material: Handle<ColorMaterial>,
}

/// 把构建好的层生成实体（主线程）。`origin` 为网格局部坐标原点的世界位置。
pub fn spawn_map_layers_at(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<ColorMaterial>,
    layers: Vec<MapLayer>,
    origin: Vec2,
) -> Vec<SpawnedMapLayer> {
    let mut spawned = Vec::with_capacity(layers.len());
    for layer in layers {
        let mesh: Handle<Mesh> = meshes.add(layer.mesh);
        let mat: Handle<ColorMaterial> = materials.add(ColorMaterial::from(layer.color));
        let entity = commands
            .spawn((
                Mesh2d(mesh.clone()),
                MeshMaterial2d(mat.clone()),
                Transform::from_xyz(origin.x, origin.y, layer.z),
            ))
            .id();
        spawned.push(SpawnedMapLayer { entity, mesh, material: mat });
    }
    spawned
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
    fn line_mesh_quad_count() {
        let l = line(vec![Vec2::new(0.0, 0.0), Vec2::new(100.0, 0.0), Vec2::new(100.0, 100.0)]);
        let mesh = line_mesh(&[l]);
        // 2 段 × 每段 4 顶点（双面三角形不增加顶点）
        assert_eq!(mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().len(), 8);
        // 2 段 × 每段 4 个三角形 × 3 索引
        assert_eq!(mesh.indices().unwrap().len(), 24);
    }

    #[test]
    fn graticule_produces_lines() {
        let map = MapData {
            min: Vec2::new(-5000.0, -5000.0),
            max: Vec2::new(5000.0, 5000.0),
            ..Default::default()
        };
        let proj = Projection::new(21.35, -157.92);
        let lines = graticule_lines(&map, &proj, 0.05);
        assert!(!lines.is_empty(), "应生成经纬网线");
        assert!(lines.iter().all(|l| l.pts.len() == 2));
    }
}
