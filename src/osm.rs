//! `.osm` XML 解析与地图要素提取。
//!
//! 解析 Overpass / JOSM 导出的 OSM XML（nodes / ways / relations），
//! 在给定投影下提取为战术地图图层：水域、绿地、用地、建筑（面），
//! 道路、铁路、水道、跑道（线）。multipolygon relation 会做环拼装与洞归属。

use std::collections::HashMap;

use bevy::math::Vec2;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::geo::{point_in_ring, ring_area, Projection};

// ---------- 原始数据模型 ----------

#[derive(Debug, Default)]
pub struct OsmData {
    /// node id -> (lat, lon)
    pub nodes: HashMap<i64, (f64, f64)>,
    pub ways: HashMap<i64, Way>,
    pub relations: HashMap<i64, Relation>,
}

#[derive(Debug, Default, Clone)]
pub struct Way {
    pub tags: HashMap<String, String>,
    pub node_ids: Vec<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct Relation {
    pub tags: HashMap<String, String>,
    /// 仅保留 type="way" 的成员
    pub members: Vec<(i64, String)>, // (way id, role)
}

// ---------- 提取后的图层模型 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolyKind {
    Water,
    Green,
    Landuse,
    Building,
    Apron,
}

#[derive(Debug, Clone)]
pub struct Poly {
    pub kind: PolyKind,
    pub outer: Vec<Vec2>,
    pub holes: Vec<Vec<Vec2>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoadClass {
    Major,
    Mid,
    Minor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineKind {
    Road(RoadClass),
    Rail,
    Waterway,
    Runway,
    Taxiway,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub kind: LineKind,
    pub pts: Vec<Vec2>,
    pub width: f32,
}

#[derive(Debug, Default)]
pub struct MapData {
    pub polys: Vec<Poly>,
    pub lines: Vec<Line>,
    /// 数据外接框（本地米制坐标）
    pub min: Vec2,
    pub max: Vec2,
}

// ---------- 解析 ----------

fn attr_of(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

pub fn parse_osm(xml: &str) -> Result<OsmData, String> {
    let mut data = OsmData::default();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    // 当前正在解析的元素
    enum Ctx {
        None,
        Node,
        Way(i64),
        Relation(i64),
    }
    let mut ctx = Ctx::None;

    loop {
        match reader.read_event().map_err(|e| format!("XML 解析错误: {e}"))? {
            Event::Start(e) => match e.name().as_ref() {
                b"node" => {
                    let id = attr_of(&e, b"id").and_then(|v| v.parse::<i64>().ok());
                    match id {
                        Some(id) if attr_of(&e, b"lat").is_some() && attr_of(&e, b"lon").is_some() => {
                            let lat: f64 = attr_of(&e, b"lat").unwrap().parse().unwrap_or(0.0);
                            let lon: f64 = attr_of(&e, b"lon").unwrap().parse().unwrap_or(0.0);
                            data.nodes.insert(id, (lat, lon));
                            ctx = Ctx::Node;
                        }
                        _ => ctx = Ctx::None,
                    }
                }
                b"way" => {
                    let id = attr_of(&e, b"id").and_then(|v| v.parse::<i64>().ok());
                    match id {
                        Some(id) => {
                            data.ways.entry(id).or_default();
                            ctx = Ctx::Way(id);
                        }
                        None => ctx = Ctx::None,
                    }
                }
                b"relation" => {
                    let id = attr_of(&e, b"id").and_then(|v| v.parse::<i64>().ok());
                    match id {
                        Some(id) => {
                            data.relations.entry(id).or_default();
                            ctx = Ctx::Relation(id);
                        }
                        None => ctx = Ctx::None,
                    }
                }
                _ => ctx = Ctx::None,
            },
            Event::Empty(e) => match e.name().as_ref() {
                b"node" => {
                    // Overpass/JOSM 的 node 通常自闭合
                    let (id_s, lat_s, lon_s) = (
                        attr_of(&e, b"id"),
                        attr_of(&e, b"lat"),
                        attr_of(&e, b"lon"),
                    );
                    if let (Some(id_s), Some(lat_s), Some(lon_s)) = (id_s, lat_s, lon_s) {
                        if let (Ok(id), Ok(lat), Ok(lon)) =
                            (id_s.parse::<i64>(), lat_s.parse::<f64>(), lon_s.parse::<f64>())
                        {
                            data.nodes.insert(id, (lat, lon));
                        }
                    }
                }
                b"tag" => {
                    let k = attr_of(&e, b"k").unwrap_or_default();
                    let v = attr_of(&e, b"v").unwrap_or_default();
                    if !k.is_empty() {
                        match &ctx {
                            Ctx::Way(id) => {
                                data.ways.get_mut(id).unwrap().tags.insert(k, v);
                            }
                            Ctx::Relation(id) => {
                                data.relations.get_mut(id).unwrap().tags.insert(k, v);
                            }
                            _ => {}
                        }
                    }
                }
                b"nd" => {
                    if let Ctx::Way(id) = &ctx {
                        let Some(ref_id) = attr_of(&e, b"ref").and_then(|v| v.parse::<i64>().ok()) else {
                            continue;
                        };
                        data.ways.get_mut(id).unwrap().node_ids.push(ref_id);
                    }
                }
                b"member" => {
                    if let Ctx::Relation(id) = &ctx {
                        let mtype = attr_of(&e, b"type").unwrap_or_default();
                        if mtype == "way" {
                            let Some(ref_id) = attr_of(&e, b"ref").and_then(|v| v.parse::<i64>().ok()) else {
                                continue;
                            };
                            let role = attr_of(&e, b"role").unwrap_or_default();
                            data.relations.get_mut(id).unwrap().members.push((ref_id, role));
                        }
                    }
                }
                _ => {}
            },
            Event::End(_) => ctx = Ctx::None,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(data)
}

impl OsmData {
    /// 依据全部节点求外接框中心，构造投影
    pub fn projection(&self) -> Option<Projection> {
        if self.nodes.is_empty() {
            return None;
        }
        let mut lat_min = f64::MAX;
        let mut lat_max = f64::MIN;
        let mut lon_min = f64::MAX;
        let mut lon_max = f64::MIN;
        for &(lat, lon) in self.nodes.values() {
            lat_min = lat_min.min(lat);
            lat_max = lat_max.max(lat);
            lon_min = lon_min.min(lon);
            lon_max = lon_max.max(lon);
        }
        Some(Projection::new((lat_min + lat_max) / 2.0, (lon_min + lon_max) / 2.0))
    }
}

// ---------- 要素提取 ----------

fn tag_matches<'a>(tags: &'a HashMap<String, String>, key: &str, values: &[&'a str]) -> Option<&'a str> {
    tags.get(key).and_then(|v| values.iter().find(|kv| v == **kv).map(|s| *s))
}

/// 闭合 way → 环（去掉首尾重复点）
fn closed_ring(way: &Way, xy: &dyn Fn(i64) -> Option<Vec2>) -> Option<Vec<Vec2>> {
    let ids = &way.node_ids;
    if ids.len() < 4 || ids.first() != ids.last() {
        return None;
    }
    let mut pts = Vec::with_capacity(ids.len() - 1);
    for &id in &ids[..ids.len() - 1] {
        let p = xy(id)?;
        if pts.last() != Some(&p) {
            pts.push(p);
        }
    }
    if pts.len() >= 3 {
        Some(pts)
    } else {
        None
    }
}

fn open_points(way: &Way, xy: &dyn Fn(i64) -> Option<Vec2>) -> Option<Vec<Vec2>> {
    let mut pts = Vec::with_capacity(way.node_ids.len());
    for &id in &way.node_ids {
        let p = xy(id)?;
        if pts.last() != Some(&p) {
            pts.push(p);
        }
    }
    if pts.len() >= 2 {
        Some(pts)
    } else {
        None
    }
}

/// 将 multipolygon 成员 way 拼装为闭合环（贪心首尾衔接）
fn assemble_rings(way_ids: &[i64], data: &OsmData, xy: &dyn Fn(i64) -> Option<Vec2>) -> Vec<Vec<Vec2>> {
    let mut chains: Vec<Vec<Vec2>> = Vec::new();
    for &wid in way_ids {
        if let Some(w) = data.ways.get(&wid) {
            if let Some(pts) = open_points(w, xy) {
                chains.push(pts);
            }
        }
    }
    let mut rings = Vec::new();
    while let Some(mut chain) = chains.pop() {
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 10_000 {
                break; // 防御性保护，正常 relation 远小于此
            }
            if chain.len() >= 4 && chain.first() == chain.last() {
                chain.pop();
                rings.push(chain);
                break;
            }
            let tail = *chain.last().unwrap();
            let mut merged = false;
            for i in 0..chains.len() {
                let cand = chains[i].clone();
                let head = *cand.first().unwrap();
                let ctail = *cand.last().unwrap();
                let eps = 0.01;
                if tail.distance_squared(head) < eps {
                    chain.extend_from_slice(&cand[1..]);
                    chains.remove(i);
                    merged = true;
                    break;
                } else if tail.distance_squared(ctail) < eps {
                    chain.extend(cand[..cand.len() - 1].iter().rev());
                    chains.remove(i);
                    merged = true;
                    break;
                }
            }
            if !merged {
                break; // 无法继续闭合，丢弃该链
            }
        }
    }
    rings
}

/// 归一化方向：outer 逆时针（正面积），洞顺时针
fn normalize_ring(ring: &mut Vec<Vec2>) {
    if ring_area(ring) < 0.0 {
        ring.reverse();
    }
}
fn normalize_hole(ring: &mut Vec<Vec2>) {
    if ring_area(ring) > 0.0 {
        ring.reverse();
    }
}

fn poly_from_rings(kind: PolyKind, outers: Vec<Vec<Vec2>>, inners: Vec<Vec<Vec2>>) -> Vec<Poly> {
    // 每个洞归属到包含它的最小外环
    outers
        .into_iter()
        .map(|mut outer| {
            normalize_ring(&mut outer);
            let area = ring_area(&outer).abs();
            let mut holes = Vec::new();
            for mut hole in inners.iter().cloned() {
                let probe = hole[0];
                if point_in_ring(probe, &outer) {
                    normalize_hole(&mut hole);
                    holes.push(hole);
                }
            }
            let _ = area;
            Poly { kind, outer, holes }
        })
        .collect()
}

pub fn extract_map(data: &OsmData, proj: &Projection) -> MapData {
    let node_xy = |id: i64| -> Option<Vec2> {
        data.nodes.get(&id).map(|&(lat, lon)| proj.project(lat, lon))
    };

    let mut map = MapData::default();
    let mut min = Vec2::splat(f32::MAX);
    let mut max = Vec2::splat(f32::MIN);

    let mut acc = |p: Vec2| {
        min = min.min(p);
        max = max.max(p);
    };

    // --- ways ---
    let mut way_ids: Vec<i64> = data.ways.keys().copied().collect();
    way_ids.sort_unstable();
    for wid in way_ids {
        let way = &data.ways[&wid];
        let t = &way.tags;

        if t.contains_key("building") {
            if let Some(mut ring) = closed_ring(way, &node_xy) {
                normalize_ring(&mut ring);
                ring.iter().for_each(|&p| acc(p));
                map.polys.push(Poly { kind: PolyKind::Building, outer: ring, holes: vec![] });
            }
            continue;
        }

        if t.get("natural").map(String::as_str) == Some("water") {
            if let Some(mut ring) = closed_ring(way, &node_xy) {
                normalize_ring(&mut ring);
                ring.iter().for_each(|&p| acc(p));
                map.polys.push(Poly { kind: PolyKind::Water, outer: ring, holes: vec![] });
            }
            continue;
        }

        if let Some(v) = t.get("waterway") {
            let width = match v.as_str() {
                "riverbank" | "dock" => {
                    if let Some(mut ring) = closed_ring(way, &node_xy) {
                        normalize_ring(&mut ring);
                        ring.iter().for_each(|&p| acc(p));
                        map.polys.push(Poly { kind: PolyKind::Water, outer: ring, holes: vec![] });
                        continue;
                    }
                    40.0
                }
                "river" => 40.0,
                "canal" => 24.0,
                "stream" => 10.0,
                _ => continue,
            };
            if let Some(pts) = open_points(way, &node_xy) {
                pts.iter().for_each(|&p| acc(p));
                map.lines.push(Line { kind: LineKind::Waterway, pts, width });
            }
            continue;
        }

        let green = tag_matches(t, "natural", &["wood", "scrub", "wetland", "beach", "sand"]).is_some()
            || tag_matches(
                t,
                "landuse",
                &["forest", "grass", "meadow", "village_green", "recreation_ground", "cemetery"],
            )
            .is_some()
            || tag_matches(t, "leisure", &["park", "golf_course", "pitch", "stadium", "playground"])
                .is_some();
        if green {
            if let Some(mut ring) = closed_ring(way, &node_xy) {
                normalize_ring(&mut ring);
                ring.iter().for_each(|&p| acc(p));
                map.polys.push(Poly { kind: PolyKind::Green, outer: ring, holes: vec![] });
            }
            continue;
        }

        let landuse = tag_matches(
            t,
            "landuse",
            &["residential", "military", "industrial", "commercial", "farmland", "quarry", "education"],
        )
        .is_some();
        if landuse {
            if let Some(mut ring) = closed_ring(way, &node_xy) {
                normalize_ring(&mut ring);
                ring.iter().for_each(|&p| acc(p));
                map.polys.push(Poly { kind: PolyKind::Landuse, outer: ring, holes: vec![] });
            }
            continue;
        }

        if let Some(v) = t.get("aeroway") {
            match v.as_str() {
                "runway" => {
                    if let Some(pts) = open_points(way, &node_xy) {
                        pts.iter().for_each(|&p| acc(p));
                        map.lines.push(Line { kind: LineKind::Runway, pts, width: 55.0 });
                    }
                }
                "taxiway" => {
                    if let Some(pts) = open_points(way, &node_xy) {
                        pts.iter().for_each(|&p| acc(p));
                        map.lines.push(Line { kind: LineKind::Taxiway, pts, width: 22.0 });
                    }
                }
                "apron" => {
                    if let Some(mut ring) = closed_ring(way, &node_xy) {
                        normalize_ring(&mut ring);
                        ring.iter().for_each(|&p| acc(p));
                        map.polys.push(Poly { kind: PolyKind::Apron, outer: ring, holes: vec![] });
                    }
                }
                _ => {}
            }
            continue;
        }

        if tag_matches(t, "railway", &["rail", "light_rail", "tram", "subway"]).is_some() {
            if let Some(pts) = open_points(way, &node_xy) {
                pts.iter().for_each(|&p| acc(p));
                map.lines.push(Line { kind: LineKind::Rail, pts, width: 5.0 });
            }
            continue;
        }

        if let Some(hw) = t.get("highway") {
            let (kind, width) = match hw.as_str() {
                "motorway" | "trunk" | "primary" | "motorway_link" | "trunk_link" | "primary_link" => {
                    (RoadClass::Major, 20.0)
                }
                "secondary" | "tertiary" | "secondary_link" | "tertiary_link" => (RoadClass::Mid, 14.0),
                "residential" | "unclassified" | "living_street" | "service" | "road" => {
                    (RoadClass::Minor, 10.0)
                }
                // 步道细节在战术视角下是噪声
                _ => continue,
            };
            if let Some(pts) = open_points(way, &node_xy) {
                pts.iter().for_each(|&p| acc(p));
                map.lines.push(Line { kind: LineKind::Road(kind), pts, width });
            }
        }
    }

    // --- multipolygon relations ---
    let mut rel_ids: Vec<i64> = data.relations.keys().copied().collect();
    rel_ids.sort_unstable();
    for rid in rel_ids {
        let rel = &data.relations[&rid];
        if rel.tags.get("type").map(String::as_str) != Some("multipolygon") {
            continue;
        }
        let t = &rel.tags;
        let kind = if t.get("natural").map(String::as_str) == Some("water") {
            PolyKind::Water
        } else if tag_matches(
            t,
            "natural",
            &["wood", "scrub", "wetland", "beach", "sand"],
        )
        .is_some()
            || tag_matches(
                t,
                "landuse",
                &["forest", "grass", "meadow", "village_green", "recreation_ground", "cemetery"],
            )
            .is_some()
            || tag_matches(t, "leisure", &["park", "golf_course", "pitch", "stadium", "playground"])
                .is_some()
        {
            PolyKind::Green
        } else if tag_matches(
            t,
            "landuse",
            &["residential", "military", "industrial", "commercial", "farmland", "quarry", "education"],
        )
        .is_some()
        {
            PolyKind::Landuse
        } else {
            continue;
        };
        let outer_ids: Vec<i64> = rel
            .members
            .iter()
            .filter(|(_, role)| role.is_empty() || role == "outer")
            .map(|&(id, _)| id)
            .collect();
        let inner_ids: Vec<i64> =
            rel.members.iter().filter(|(_, role)| role == "inner").map(|&(id, _)| id).collect();
        let outers: Vec<Vec<Vec2>> = assemble_rings(&outer_ids, data, &node_xy)
            .into_iter()
            .filter(|r| ring_area(r).abs() > 500.0) // 过滤碎屑
            .collect();
        let inners: Vec<Vec<Vec2>> = assemble_rings(&inner_ids, data, &node_xy)
            .into_iter()
            .filter(|r| ring_area(r).abs() > 500.0)
            .collect();
        if outers.is_empty() {
            continue;
        }
        for ring in outers.iter().chain(inners.iter()) {
            ring.iter().for_each(|&p| acc(p));
        }
        map.polys.extend(poly_from_rings(kind, outers, inners));
    }

    if min.x <= max.x {
        map.min = min;
        map.max = max;
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osm version="0.6">
  <node id="1" lat="0.00" lon="0.00"/>
  <node id="2" lat="0.01" lon="0.00"/>
  <node id="3" lat="0.01" lon="0.01"/>
  <node id="4" lat="0.00" lon="0.01"/>
  <node id="5" lat="0.02" lon="0.02"/>
  <node id="6" lat="0.02" lon="0.03"/>
  <node id="7" lat="0.03" lon="0.03"/>
  <node id="8" lat="0.03" lon="0.02"/>
  <way id="101">
    <tag k="building" v="yes"/>
    <nd ref="1"/><nd ref="2"/><nd ref="3"/><nd ref="4"/><nd ref="1"/>
  </way>
  <way id="102">
    <tag k="highway" v="primary"/>
    <nd ref="1"/><nd ref="5"/>
  </way>
  <relation id="201">
    <tag k="type" v="multipolygon"/>
    <tag k="natural" v="water"/>
    <member type="way" ref="103" role="outer"/>
    <member type="way" ref="104" role="outer"/>
  </relation>
  <way id="103">
    <nd ref="5"/><nd ref="6"/>
  </way>
  <way id="104">
    <nd ref="6"/><nd ref="7"/><nd ref="8"/><nd ref="5"/>
  </way>
</osm>"#;

    #[test]
    fn parse_and_extract() {
        let data = parse_osm(SAMPLE).expect("解析失败");
        assert_eq!(data.nodes.len(), 8);
        assert_eq!(data.ways.len(), 4);
        assert_eq!(data.relations.len(), 1);
        assert_eq!(data.ways[&102].tags["highway"], "primary");
        assert_eq!(data.relations[&201].members.len(), 2);

        let proj = data.projection().expect("应能构造投影");
        let map = extract_map(&data, &proj);
        // 建筑 1 + 水域 relation 1 = 2 个面
        assert_eq!(map.polys.iter().filter(|p| p.kind == PolyKind::Building).count(), 1);
        assert_eq!(map.polys.iter().filter(|p| p.kind == PolyKind::Water).count(), 1);
        // 道路 1 条
        assert_eq!(map.lines.len(), 1);
        assert_eq!(map.lines[0].kind, LineKind::Road(RoadClass::Major));
        // 水域环由两条 way 拼装而成，应有 4 个顶点
        let water = map.polys.iter().find(|p| p.kind == PolyKind::Water).unwrap();
        assert_eq!(water.outer.len(), 4, "拼装后的外环应为 4 点，实际 {:?}", water.outer);
    }

    #[test]
    fn assemble_two_open_ways_into_ring() {
        let data = parse_osm(SAMPLE).unwrap();
        let proj = data.projection().unwrap();
        let node_xy = |id: i64| data.nodes.get(&id).map(|&(lat, lon)| proj.project(lat, lon));
        let rings = assemble_rings(&[103, 104], &data, &node_xy);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].len(), 4);
    }
}
