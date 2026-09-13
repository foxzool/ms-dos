//! Mapbox Vector Tile（MVT, protobuf）最小解码器与 OpenMapTiles 图层映射。
//!
//! 只读不写，按需解码 Tile/Layer/Feature/Value 四种消息与几何命令流，
//! 输出为瓦片局部整数坐标（0..extent，y 向下），由调用方换算到世界米制坐标。

use bevy::math::Vec2;

use crate::osm::{Line, LineKind, MapData, Poly, PolyKind, RoadClass};

// ---------- protobuf 读取器 ----------

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn eof(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn varint(&mut self) -> u64 {
        let mut result = 0u64;
        let mut shift = 0;
        while self.pos < self.buf.len() {
            let b = self.buf[self.pos];
            self.pos += 1;
            result |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        result
    }

    /// 返回 (字段号, wire 类型)
    fn tag(&mut self) -> Option<(u32, u8)> {
        if self.eof() {
            return None;
        }
        let v = self.varint();
        Some(((v >> 3) as u32, (v & 0x7) as u8))
    }

    fn take_bytes(&mut self) -> &'a [u8] {
        let len = self.varint() as usize;
        let end = (self.pos + len).min(self.buf.len());
        let bytes = &self.buf[self.pos..end];
        self.pos = end;
        bytes
    }

    fn skip(&mut self, wire: u8) {
        match wire {
            0 => {
                self.varint();
            }
            1 => self.pos = (self.pos + 8).min(self.buf.len()),
            2 => {
                let len = self.varint() as usize;
                self.pos = (self.pos + len).min(self.buf.len());
            }
            5 => self.pos = (self.pos + 4).min(self.buf.len()),
            _ => self.pos = self.buf.len(),
        }
    }
}

fn zigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

// ---------- MVT 模型 ----------

pub struct MvtFeature {
    /// 1=点 2=线 3=面
    pub geom_type: u32,
    pub geometry: Vec<u32>,
    pub tags: Vec<(String, String)>,
}

pub struct MvtLayer {
    pub name: String,
    #[allow(dead_code)] // 预留：按层 extent 换算（当前 OMT 全层统一 4096）
    pub extent: u32,
    pub features: Vec<MvtFeature>,
}

pub struct MvtTile {
    pub layers: Vec<MvtLayer>,
}

/// 解码 Value 消息为字符串（数值型值转为十进制字符串）
fn decode_value(buf: &[u8]) -> String {
    let mut r = Reader::new(buf);
    while let Some((field, wire)) = r.tag() {
        match (field, wire) {
            (1, 2) => return String::from_utf8_lossy(r.take_bytes()).into_owned(),
            (2, 5) => {
                let b = r.take_bytes();
                if b.len() >= 4 {
                    return format!("{}", f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                }
            }
            (3, 1) => {
                let b = r.take_bytes();
                if b.len() >= 8 {
                    return format!("{}", f64::from_le_bytes(b[..8].try_into().unwrap()));
                }
            }
            (4, 0) => return r.varint().to_string(),
            (5, 0) => return r.varint().to_string(),
            (6, 0) => return zigzag(r.varint() as u32).to_string(),
            (7, 0) => return (r.varint() != 0).to_string(),
            _ => r.skip(wire),
        }
    }
    String::new()
}

fn decode_feature(buf: &[u8], keys: &[String], values: &[String]) -> MvtFeature {
    let mut geom_type = 0u32;
    let mut geometry: Vec<u32> = Vec::new();
    let mut tag_idx: Vec<u32> = Vec::new();
    let mut r = Reader::new(buf);
    while let Some((field, wire)) = r.tag() {
        match (field, wire) {
            (2, 2) => {
                // tags：packed varint
                let mut rr = Reader::new(r.take_bytes());
                while !rr.eof() {
                    tag_idx.push(rr.varint() as u32);
                }
            }
            (3, 0) => geom_type = r.varint() as u32,
            (4, 2) => {
                let mut rr = Reader::new(r.take_bytes());
                while !rr.eof() {
                    geometry.push(rr.varint() as u32);
                }
            }
            _ => r.skip(wire),
        }
    }
    let tags = tag_idx
        .chunks_exact(2)
        .filter_map(|pair| {
            keys.get(pair[0] as usize).cloned().map(|k| {
                let v = values.get(pair[1] as usize).cloned().unwrap_or_default();
                (k, v)
            })
        })
        .collect();
    MvtFeature { geom_type, geometry, tags }
}

fn decode_layer(buf: &[u8]) -> MvtLayer {
    let mut name = String::new();
    let mut extent = 4096u32;
    let mut features = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    let mut raw_features: Vec<&[u8]> = Vec::new();
    let mut r = Reader::new(buf);
    while let Some((field, wire)) = r.tag() {
        match (field, wire) {
            (1, 2) => name = String::from_utf8_lossy(r.take_bytes()).into_owned(),
            (2, 2) => raw_features.push(r.take_bytes()),
            (3, 2) => keys.push(String::from_utf8_lossy(r.take_bytes()).into_owned()),
            (4, 2) => values.push(decode_value(r.take_bytes())),
            (5, 0) => extent = r.varint() as u32,
            _ => r.skip(wire),
        }
    }
    for f in raw_features {
        features.push(decode_feature(f, &keys, &values));
    }
    MvtLayer { name, extent, features }
}

/// 解码整块 MVT
pub fn decode_mvt(buf: &[u8]) -> MvtTile {
    // 部分瓦片以 gzip 存储（magic 1f 8b）：桌面/浏览器端解压由调用方处理
    let mut layers = Vec::new();
    let mut r = Reader::new(buf);
    while let Some((field, wire)) = r.tag() {
        if field == 3 && wire == 2 {
            layers.push(decode_layer(r.take_bytes()));
        } else {
            r.skip(wire);
        }
    }
    MvtTile { layers }
}

// ---------- 几何命令流 → 环 ----------

/// 解析几何命令流为坐标环（瓦片局部整数坐标，y 向下）
pub fn decode_rings(geometry: &[u32]) -> Vec<Vec<(i32, i32)>> {
    let mut rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut current: Vec<(i32, i32)> = Vec::new();
    let mut x = 0i32;
    let mut y = 0i32;
    let mut i = 0usize;
    while i < geometry.len() {
        let cmd = geometry[i];
        let id = cmd & 0x7;
        let count = (cmd >> 3) as usize;
        i += 1;
        match id {
            1 => {
                // MoveTo：开启新环（或新要素）
                for _ in 0..count {
                    if i + 1 >= geometry.len() + 1 {
                        break;
                    }
                    x += zigzag(geometry[i]);
                    y += zigzag(geometry[i + 1]);
                    i += 2;
                    if !current.is_empty() {
                        rings.push(std::mem::take(&mut current));
                    }
                    current.push((x, y));
                }
            }
            2 => {
                // LineTo
                for _ in 0..count {
                    if i + 1 >= geometry.len() + 1 {
                        break;
                    }
                    x += zigzag(geometry[i]);
                    y += zigzag(geometry[i + 1]);
                    i += 2;
                    current.push((x, y));
                }
            }
            7 => {
                // ClosePath
                if let Some(first) = current.first().copied() {
                    current.push(first);
                    rings.push(std::mem::take(&mut current));
                }
            }
            _ => break,
        }
    }
    if !current.is_empty() {
        rings.push(current);
    }
    rings
}

// ---------- OpenMapTiles 图层映射 ----------

fn prop<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// landuse/landcover class → 我们的图层分类
fn green_kind(class: &str) -> bool {
    matches!(
        class,
        "wood" | "forest" | "grass" | "park" | "golf_course" | "cemetery" | "scrub" | "wetland" | "farmland"
    )
}

fn landuse_kind(class: &str) -> bool {
    matches!(
        class,
        "residential" | "military" | "industrial" | "commercial" | "retail" | "quarry" | "education"
    )
}

fn road_of(class: &str) -> Option<(RoadClass, f32)> {
    match class {
        "motorway" | "trunk" | "primary" | "motorway_link" | "trunk_link" | "primary_link" => {
            Some((RoadClass::Major, 20.0))
        }
        "secondary" | "tertiary" | "secondary_link" | "tertiary_link" => Some((RoadClass::Mid, 14.0)),
        "minor" | "service" | "residential" | "unclassified" | "living_street" | "road" => {
            Some((RoadClass::Minor, 10.0))
        }
        _ => None,
    }
}

fn waterway_width(class: &str) -> Option<f32> {
    match class {
        "river" => Some(40.0),
        "canal" => Some(24.0),
        "stream" => Some(10.0),
        "riverbank" | "dock" => Some(40.0),
        _ => None,
    }
}

/// 将 MVT 瓦片转换为渲染管线的 MapData。
/// `world` 闭包：瓦片局部 extent 坐标 (x, y向下) → 世界米制坐标（局部于瓦片中心原点）。
pub fn mvt_to_mapdata<F: Fn((i32, i32)) -> Vec2>(tile: &MvtTile, world: F) -> MapData {
    let mut polys: Vec<Poly> = Vec::new();
    let mut lines: Vec<Line> = Vec::new();
    for layer in &tile.layers {
        for f in &layer.features {
            match (layer.name.as_str(), f.geom_type) {
                ("water", 3) | ("water", 2) => {
                    if f.geom_type == 3 {
                        push_poly(&mut polys, &f.geometry, PolyKind::Water, &world);
                    }
                }
                ("waterway", 2) => {
                    let width = prop(&f.tags, "class")
                        .and_then(waterway_width)
                        .unwrap_or(20.0);
                    push_line(&mut lines, &f.geometry, LineKind::Waterway, width, &world);
                }
                ("landcover", 3) | ("park", 3) => {
                    if prop(&f.tags, "class").map_or(false, green_kind)
                        || layer.name == "park"
                    {
                        push_poly(&mut polys, &f.geometry, PolyKind::Green, &world);
                    }
                }
                ("landuse", 3) => {
                    let class = prop(&f.tags, "class").unwrap_or("");
                    if green_kind(class) {
                        push_poly(&mut polys, &f.geometry, PolyKind::Green, &world);
                    } else if landuse_kind(class) {
                        push_poly(&mut polys, &f.geometry, PolyKind::Landuse, &world);
                    }
                }
                ("building", 3) => push_poly(&mut polys, &f.geometry, PolyKind::Building, &world),
                ("transportation", 2) => {
                    let class = prop(&f.tags, "class").unwrap_or("");
                    let aeroway = prop(&f.tags, "aeroway");
                    if aeroway == Some("runway") || class == "runway" {
                        push_line(&mut lines, &f.geometry, LineKind::Runway, 55.0, &world);
                    } else if aeroway == Some("taxiway") || class == "taxiway" {
                        push_line(&mut lines, &f.geometry, LineKind::Taxiway, 22.0, &world);
                    } else if let Some((rc, w)) = road_of(class) {
                        push_line(&mut lines, &f.geometry, LineKind::Road(rc), w, &world);
                    }
                }
                _ => {}
            }
        }
    }
    MapData { polys, lines, min: Vec2::ZERO, max: Vec2::ZERO }
}

fn push_poly<F: Fn((i32, i32)) -> Vec2>(polys: &mut Vec<Poly>, geometry: &[u32], kind: PolyKind, world: &F) {
    let rings = decode_rings(geometry);
    if rings.is_empty() {
        return;
    }
    // OMT 环顺序：首环为外环，其后为洞（面积符号判断归类更稳）
    let mut outer: Vec<Vec2> = Vec::new();
    let mut holes: Vec<Vec<Vec2>> = Vec::new();
    for (i, ring) in rings.iter().enumerate() {
        let pts: Vec<Vec2> = ring.iter().map(|&p| world(p)).collect();
        if i == 0 {
            outer = pts;
        } else if crate::geo::ring_area(&pts).abs() < crate::geo::ring_area(&outer).abs() {
            holes.push(pts);
        }
    }
    if outer.len() >= 3 {
        polys.push(Poly { kind, outer, holes });
    }
}

fn push_line<F: Fn((i32, i32)) -> Vec2>(
    lines: &mut Vec<Line>,
    geometry: &[u32],
    kind: LineKind,
    width: f32,
    world: &F,
) {
    for ring in decode_rings(geometry) {
        let pts: Vec<Vec2> = ring.iter().map(|&p| world(p)).collect();
        if pts.len() >= 2 {
            lines.push(Line { kind, pts, width });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小 protobuf：field(varint/string/message) 编码辅助
    fn tag(field: u32, wire: u8) -> Vec<u8> {
        vec![((field << 3) | wire as u32) as u8]
    }
    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        out
    }
    fn bytes_field(field: u32, data: &[u8]) -> Vec<u8> {
        let mut out = tag(field, 2);
        out.extend(varint(data.len() as u64));
        out.extend_from_slice(data);
        out
    }
    fn varint_field(field: u32, v: u64) -> Vec<u8> {
        let mut out = tag(field, 0);
        out.extend(varint(v));
        out
    }

    #[test]
    fn decode_minimal_tile() {
        // 一个 building 层，一个正方形面要素
        // MoveTo(1)→(0,0)；LineTo(3)→(9,0)(9,9)(0,9)；ClosePath
        // zigzag: dx=9→18, dy=0；0,18；-9→17,0
        let geometry: Vec<u32> = vec![
            (1 << 3) | 1, 0, 0,
            (3 << 3) | 2, 18, 0, 0, 18, 17, 0,
            (1 << 3) | 7,
        ];
        let mut feature = Vec::new();
        let mut geom_packed = Vec::new();
        for v in &geometry {
            geom_packed.extend(varint(*v as u64));
        }
        feature.extend(bytes_field(2, &[0, 0]));
        feature.extend(varint_field(3, 3));
        feature.extend(bytes_field(4, &geom_packed));

        let mut layer = Vec::new();
        layer.extend(bytes_field(1, b"building")); // name
        layer.extend(bytes_field(2, &feature)); // features
        layer.extend(bytes_field(3, b"building")); // keys
        layer.extend(bytes_field(4, &[0x0a, 0x03, b'y', b'e', b's'])); // value "yes"
        layer.extend(varint_field(5, 4096)); // extent

        let mut tile = Vec::new();
        tile.extend(bytes_field(3, &layer)); // Tile.layers

        let decoded = decode_mvt(&tile);
        assert_eq!(decoded.layers.len(), 1);
        assert_eq!(decoded.layers[0].name, "building");
        assert_eq!(decoded.layers[0].extent, 4096);
        let f = &decoded.layers[0].features[0];
        assert_eq!(f.geom_type, 3);
        assert_eq!(f.tags, vec![("building".to_string(), "yes".to_string())]);

        let rings = decode_rings(&f.geometry);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0][0], (0, 0));
        assert_eq!(rings[0][1], (9, 0));
        assert_eq!(rings[0][2], (9, 9));
        assert_eq!(rings[0][3], (0, 9));
        assert_eq!(rings[0][4], (0, 0)); // 闭合
    }

    #[test]
    fn zigzag_roundtrip() {
        for v in [-100i32, -1, 0, 1, 99, 4096] {
            let z = ((v << 1) ^ (v >> 31)) as u32;
            assert_eq!(zigzag(z), v);
        }
    }

    #[test]
    fn layer_mapping() {
        let mk = |name: &str, class: Option<&str>| MvtLayer {
            name: name.into(),
            extent: 4096,
            features: vec![MvtFeature {
                geom_type: 3,
                geometry: vec![(1 << 3) | 1, 0, 0, (3 << 3) | 2, 18, 0, 0, 18, 17, 0, (1 << 3) | 7],
                tags: class.map(|c| vec![("class".to_string(), c.to_string())]).unwrap_or_default(),
            }],
        };
        let world = |p: (i32, i32)| Vec2::new(p.0 as f32, p.1 as f32);
        let tile = MvtTile {
            layers: vec![
                mk("water", None),
                mk("landuse", Some("residential")),
                mk("landuse", Some("forest")),
                mk("park", None),
            ],
        };
        let map = mvt_to_mapdata(&tile, world);
        assert_eq!(map.polys.iter().filter(|p| p.kind == PolyKind::Water).count(), 1);
        assert_eq!(map.polys.iter().filter(|p| p.kind == PolyKind::Landuse).count(), 1);
        assert_eq!(map.polys.iter().filter(|p| p.kind == PolyKind::Green).count(), 2);
    }

    #[test]
    fn road_class_mapping() {
        assert!(matches!(road_of("motorway"), Some((RoadClass::Major, _))));
        assert!(matches!(road_of("secondary"), Some((RoadClass::Mid, _))));
        assert!(matches!(road_of("residential"), Some((RoadClass::Minor, _))));
        assert!(road_of("rail").is_none());
        assert_eq!(waterway_width("river"), Some(40.0));
    }
}
