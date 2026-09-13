//! 地理投影与平面几何工具。
//!
//! 世界坐标约定：1 world unit = 1 米，x 向东，y 向北（与 Bevy 2D 的 y 朝上一致）。
//! 采用球面 Web Mercator（EPSG:3857 同族公式），以 (lat0, lon0) 为原点做平移——
//! 这与 OSM 瓦片体系一致，且全球坐标统一，支撑任意地点的按需瓦片加载。
//! 纬度截断在 ±85.05°（与 Web Mercator 标准一致）。

use bevy::math::Vec2;

const MERC_R: f64 = 6_378_137.0;
const MAX_LAT: f64 = 85.051_128_78;

fn merc_y(lat_deg: f64) -> f64 {
    let lat = lat_deg.clamp(-MAX_LAT, MAX_LAT).to_radians();
    MERC_R * (std::f64::consts::FRAC_PI_4 + lat * 0.5).tan().ln()
}

fn inv_merc_y(y: f64) -> f64 {
    (2.0 * (y / MERC_R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees()
}

/// 以 (lat0, lon0) 为原点的米制投影（Web Mercator 平移）。
#[derive(Debug, Clone, Copy)]
pub struct Projection {
    pub lat0: f64,
    pub lon0: f64,
}

impl Projection {
    pub fn new(lat0: f64, lon0: f64) -> Self {
        Projection { lat0, lon0 }
    }

    /// 全球统一坐标（原点为经纬 (0,0)）
    pub fn global() -> Self {
        Projection { lat0: 0.0, lon0: 0.0 }
    }

    /// 经纬度 → 平面坐标（米）
    pub fn project(&self, lat: f64, lon: f64) -> Vec2 {
        Vec2::new(
            ((lon - self.lon0).to_radians() * MERC_R) as f32,
            (merc_y(lat) - merc_y(self.lat0)) as f32,
        )
    }

    /// 平面坐标（米）→ 经纬度
    pub fn unproject(&self, p: Vec2) -> (f64, f64) {
        (
            inv_merc_y(merc_y(self.lat0) + p.y as f64),
            self.lon0 + (p.x as f64 / MERC_R).to_degrees(),
        )
    }
}

/// 有向环面积（鞋带公式），用于判定环方向。单位：平方米。
pub fn ring_area(ring: &[Vec2]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        sum += (a.x as f64) * (b.y as f64) - (b.x as f64) * (a.y as f64);
    }
    sum / 2.0
}

/// 射线法点包含测试（不含洞，由调用方处理洞）。
pub fn point_in_ring(p: Vec2, ring: &[Vec2]) -> bool {
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let pi = ring[i];
        let pj = ring[j];
        if (pi.y > p.y) != (pj.y > p.y)
            && p.x < (pj.x - pi.x) * (p.y - pi.y) / (pj.y - pi.y) + pi.x
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// 距离/方位角（度，北为 0，顺时针）工具，供 UI 显示。
pub fn bearing_deg(from: Vec2, to: Vec2) -> f32 {
    let d = to - from;
    let mut b = d.x.atan2(d.y).to_degrees();
    if b < 0.0 {
        b += 360.0;
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_roundtrip() {
        for &(lat0, lon0, lat, lon) in &[
            (21.355, -157.925, 21.400, -157.900),
            (0.0, 0.0, -33.86, 151.21),
            (21.355, -157.925, 60.0, 120.0),
            (0.0, 0.0, 85.0, -179.9),
        ] {
            let proj = Projection::new(lat0, lon0);
            let p = proj.project(lat, lon);
            let (lat2, lon2) = proj.unproject(p);
            assert!((lat2 - lat).abs() < 1e-5, "lat 误差过大: {lat2} vs {lat}");
            assert!((lon2 - lon).abs() < 1e-5, "lon 误差过大: {lon2} vs {lon}");
        }
    }

    #[test]
    fn projection_scale_sane() {
        // Web Mercator：每度经度恒等于 2πR/360 ≈ 111.32 km（任意纬度）
        let proj = Projection::global();
        let a = proj.project(21.0, -158.0);
        let b = proj.project(21.0, -157.99);
        assert!((b.x - a.x - 1113.2).abs() < 2.0, "m/deg lon = {}", b.x - a.x);
        // 局部每度纬度 ≈ 111.32km / cos(lat)
        let c = proj.project(21.01, -158.0);
        let expect = 1113.19 / 21.0_f64.to_radians().cos();
        assert!((c.y - a.y - expect as f32).abs() < 5.0, "m/deg lat = {}", c.y - a.y);
        // 全球范围
        let hi = proj.project(85.05, 180.0);
        assert!(hi.x > 20_000_000.0 && hi.y > 20_000_000.0);
    }

    #[test]
    fn ring_area_and_point_inclusion() {
        let square = vec![Vec2::new(0.0, 0.0), Vec2::new(10.0, 0.0), Vec2::new(10.0, 10.0), Vec2::new(0.0, 10.0)];
        assert!((ring_area(&square) - 100.0).abs() < 1e-6);
        assert!(point_in_ring(Vec2::new(5.0, 5.0), &square));
        assert!(!point_in_ring(Vec2::new(15.0, 5.0), &square));
        // 顶点与边缘附近的点也应有稳定结果
        assert!(!point_in_ring(Vec2::new(-0.5, -0.5), &square));
    }

    #[test]
    fn bearing() {
        let a = Vec2::ZERO;
        assert!((bearing_deg(a, Vec2::new(0.0, 1.0)) - 0.0).abs() < 1e-3);
        assert!((bearing_deg(a, Vec2::new(1.0, 0.0)) - 90.0).abs() < 1e-3);
        assert!((bearing_deg(a, Vec2::new(-1.0, -0.1)) - 264.3).abs() < 0.2);
    }
}
