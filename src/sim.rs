//! 仿真域：单位数据模型、航路点运动、探测判定、时钟倍速。
//!
//! 运动与探测的核心逻辑写成纯函数，便于单元测试与确定性运行；
//! Bevy 系统只做薄薄的胶水（读取组件、调用纯函数、写回）。

use bevy::prelude::*;

use crate::geo::Projection;

// ---------- 阵营与平台 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Blue,
    Red,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    Surface,
    Air,
    Subsurface,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformKind {
    Destroyer,
    Frigate,
    Submarine,
    MPatrol,
    Fighter,
    Facility,
    Merchant,
}

impl PlatformKind {
    pub fn domain(self) -> Domain {
        match self {
            PlatformKind::Destroyer | PlatformKind::Frigate | PlatformKind::Merchant => Domain::Surface,
            PlatformKind::MPatrol | PlatformKind::Fighter => Domain::Air,
            PlatformKind::Submarine => Domain::Subsurface,
            PlatformKind::Facility => Domain::Surface,
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            PlatformKind::Destroyer => "DDG",
            PlatformKind::Frigate => "FFG",
            PlatformKind::Submarine => "SSK",
            PlatformKind::MPatrol => "MPA",
            PlatformKind::Fighter => "FTR",
            PlatformKind::Facility => "FAC",
            PlatformKind::Merchant => "MV",
        }
    }

    pub fn class_name(self) -> &'static str {
        match self {
            PlatformKind::Destroyer => "Guided-missile Destroyer",
            PlatformKind::Frigate => "Frigate",
            PlatformKind::Submarine => "Attack Submarine",
            PlatformKind::MPatrol => "Maritime Patrol Aircraft",
            PlatformKind::Fighter => "Multirole Fighter",
            PlatformKind::Facility => "Fixed Facility",
            PlatformKind::Merchant => "Merchant Vessel",
        }
    }

    pub fn cruise_kts(self) -> f32 {
        match self {
            PlatformKind::Destroyer => 16.0,
            PlatformKind::Frigate => 14.0,
            PlatformKind::Submarine => 8.0,
            PlatformKind::MPatrol => 260.0,
            PlatformKind::Fighter => 350.0,
            PlatformKind::Facility => 0.0,
            PlatformKind::Merchant => 13.0,
        }
    }

    /// 转向速率（度/秒）
    pub fn turn_rate_dps(self) -> f32 {
        match self {
            PlatformKind::Destroyer | PlatformKind::Frigate => 1.5,
            PlatformKind::Submarine => 1.0,
            PlatformKind::MPatrol => 3.0,
            PlatformKind::Fighter => 6.0,
            PlatformKind::Facility | PlatformKind::Merchant => 0.0,
        }
    }

    pub fn arrive_radius_m(self) -> f32 {
        match self {
            PlatformKind::Fighter | PlatformKind::MPatrol => 900.0,
            _ => 350.0,
        }
    }

    pub fn mobile(self) -> bool {
        self != PlatformKind::Facility
    }
}

pub const KTS_TO_MPS: f32 = 0.5144;

// ---------- 探测 ----------

#[derive(Debug, Clone, Copy, Default)]
pub struct Sensors {
    /// 雷达：对空/水面
    pub radar_m: Option<f32>,
    /// 声纳：对潜/水面
    pub sonar_m: Option<f32>,
    /// 目视/光电：对空/水面
    pub visual_m: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DetectResult {
    None,
    /// 已探测；true = 已分类（能识别型号）
    Detected { classified: bool },
}

/// 单传感器对目标的探测判定。distance 单位米。
pub fn detect_with(sensors: &Sensors, target_domain: Domain, distance: f32) -> DetectResult {
    let mut best = 0.0f32;
    if matches!(target_domain, Domain::Air | Domain::Surface) {
        if let Some(r) = sensors.radar_m {
            best = best.max(r);
        }
        best = best.max(sensors.visual_m);
    }
    if matches!(target_domain, Domain::Subsurface | Domain::Surface) {
        if let Some(s) = sensors.sonar_m {
            best = best.max(s);
        }
    }
    if best > 0.0 && distance <= best {
        // 近于一半探测距离视为已分类
        DetectResult::Detected { classified: distance <= best * 0.5 }
    } else {
        DetectResult::None
    }
}

// ---------- 武器（v0.1 仅展示） ----------

#[derive(Debug, Clone)]
pub struct WeaponSpec {
    pub name: &'static str,
    pub range_km: f32,
}

// ---------- 单位组件 ----------

#[derive(Debug, Component)]
pub struct Unit {
    pub side: Side,
    pub kind: PlatformKind,
    pub name: String,
    pub hull: String,
    pub sensors: Sensors,
    pub weapons: Vec<WeaponSpec>,
    /// 巡逻航路点（持久）
    pub patrol: Vec<Vec2>,
    /// 当前执行航路点
    pub route: Vec<Vec2>,
    pub wp_index: usize,
    pub route_loop: bool,
    pub moving: bool,
    /// 本单位被蓝方探测到（fog of war）
    pub detected_by_blue: bool,
    /// 已分类（蓝方识别出型号）
    pub classified: bool,
    pub last_seen_t: Option<f64>,
}

/// 位置（世界米制坐标）
#[derive(Debug, Component)]
pub struct Position(pub Vec2);
/// 航向（数学角，弧度，0=正东，逆时针为正）
#[derive(Debug, Component)]
pub struct Heading(pub f32);
/// 当前速度（米/秒）
#[derive(Debug, Component)]
pub struct SpeedMps(pub f32);

// ---------- 时钟 ----------

pub const TIME_SPEEDS: [f64; 5] = [1.0, 5.0, 30.0, 120.0, 600.0];

#[derive(Debug, Resource)]
pub struct SimClock {
    /// 场景时间（秒），起点 2026-09-12 04:00:00Z
    pub t: f64,
    pub speed_idx: usize,
    pub paused: bool,
}

impl Default for SimClock {
    fn default() -> Self {
        SimClock { t: 0.0, speed_idx: 0, paused: false }
    }
}

impl SimClock {
    pub fn multiplier(&self) -> f64 {
        TIME_SPEEDS[self.speed_idx.min(TIME_SPEEDS.len() - 1)]
    }

    pub fn sim_dt(&self, real_dt: f32) -> f32 {
        if self.paused {
            0.0
        } else {
            (real_dt as f64 * self.multiplier()) as f32
        }
    }

    /// "D+2 07:33:05Z" 形式的场景时间（起点 04:00:00Z）
    pub fn format(&self) -> String {
        let total = self.t.floor() as i64 + 4 * 3600;
        let day = total.div_euclid(86_400);
        let rem = total.rem_euclid(86_400);
        let h = rem / 3600;
        let m = (rem % 3600) / 60;
        let s = rem % 60;
        format!("D+{day} {h:02}:{m:02}:{s:02}Z")
    }
}

// ---------- 运动纯函数 ----------

/// 把当前航向朝目标航向旋转，不超过 max_turn（弧度）
pub fn steer_turn(current: f32, desired: f32, max_turn: f32) -> f32 {
    let mut diff = desired - current;
    // 归一化到 (-PI, PI]
    while diff > std::f32::consts::PI {
        diff -= std::f32::consts::TAU;
    }
    while diff <= -std::f32::consts::PI {
        diff += std::f32::consts::TAU;
    }
    current + diff.clamp(-max_turn, max_turn)
}

/// 推进一个单位。返回是否抵达当前航路点。
pub fn step_unit(
    unit: &mut Unit,
    pos: &mut Vec2,
    heading: &mut f32,
    speed: &mut f32,
    dt: f32,
) -> bool {
    if dt <= 0.0 || !unit.moving || unit.route.is_empty() {
        *speed = if unit.moving { *speed } else { 0.0 };
        return false;
    }
    let target = unit.route[unit.wp_index.min(unit.route.len() - 1)];
    let to_target = target - *pos;
    let dist = to_target.length();
    let arrive_r = unit.kind.arrive_radius_m();
    if dist <= arrive_r {
        return true;
    }
    let desired = to_target.y.atan2(to_target.x);
    *heading = steer_turn(*heading, desired, unit.kind.turn_rate_dps().to_radians() * dt);
    // 步长截断在本航点，避免大步长穿越
    let step = (*speed * dt).min(dist);
    *pos += Vec2::new(heading.cos(), heading.sin()) * step;
    pos.distance(target) <= arrive_r
}

/// 航路点推进策略：循环巡逻或顺序单程，结束后停车。
pub fn advance_waypoint(unit: &mut Unit) {
    if unit.route_loop {
        unit.wp_index = (unit.wp_index + 1) % unit.route.len().max(1);
    } else if unit.wp_index + 1 < unit.route.len() {
        unit.wp_index += 1;
    } else {
        unit.moving = false;
    }
}

impl Unit {
    /// 恢复巡逻航线
    pub fn resume_patrol(&mut self) {
        if self.patrol.is_empty() {
            return;
        }
        self.route = self.patrol.clone();
        self.wp_index = 0;
        self.route_loop = true;
        self.moving = true;
    }

    /// 单程机动命令
    pub fn order_move(&mut self, dest: Vec2) {
        self.route = vec![dest];
        self.wp_index = 0;
        self.route_loop = false;
        self.moving = true;
    }
}

// ---------- Bevy 系统胶水 ----------

pub fn movement_system(
    mut q: Query<(&mut Unit, &mut Position, &mut Heading, &mut SpeedMps)>,
    clock: Res<SimClock>,
    time: Res<Time>,
) {
    let dt = clock.sim_dt(time.delta().as_secs_f32());
    if dt == 0.0 {
        return;
    }
    for (mut unit, mut pos, mut heading, mut speed) in &mut q {
        let arrived = step_unit(&mut unit, &mut pos.0, &mut heading.0, &mut speed.0, dt);
        if arrived {
            advance_waypoint(&mut unit);
        }
    }
}

/// 探测快照：蓝方视角的 fog of war。
/// `detected_by_blue` 表示"当前处于被探测状态"（每轮重置，驱动渲染显隐）；
/// `classified` 一旦置位则保持（识别记忆，用于 UNKNOWN → 分类升级）。
pub fn detection_system(mut q: Query<(Entity, &mut Unit, &Position)>, clock: Res<SimClock>) {
    let t = clock.t;
    for (_, mut u, _) in q.iter_mut() {
        if u.side == Side::Red {
            u.detected_by_blue = false;
        }
    }
    let snap: Vec<(Side, Sensors, Vec2)> =
        q.iter().map(|(_, u, p)| (u.side, u.sensors, p.0)).collect();
    let mut hits: Vec<(Entity, bool)> = Vec::new();
    for (_side, sensors, obs_pos) in snap.iter().filter(|(s, ..)| *s == Side::Blue) {
        for (e, u, p) in q.iter() {
            if u.side != Side::Red {
                continue;
            }
            if let DetectResult::Detected { classified } =
                detect_with(sensors, u.kind.domain(), obs_pos.distance(p.0))
            {
                hits.push((e, classified));
            }
        }
    }
    for (e, mut u, _) in q.iter_mut() {
        for (he, classified) in &hits {
            if e == *he {
                u.detected_by_blue = true;
                if *classified {
                    u.classified = true;
                }
                u.last_seen_t = Some(t);
            }
        }
    }
}

// ---------- 想定 ----------

struct UnitSpec {
    side: Side,
    kind: PlatformKind,
    name: &'static str,
    hull: &'static str,
    sensors: Sensors,
    weapons: Vec<WeaponSpec>,
    start: (f64, f64),
    heading_deg: f32,
    patrol: Vec<(f64, f64)>,
    patrol_loop: bool,
}

fn km(v: f32) -> Option<f32> {
    Some(v * 1000.0)
}

/// “珍珠港守望”想定：蓝方守备珍珠港，红方水面群自西南接近，潜艇渗透。
pub fn spawn_scenario(commands: &mut Commands, proj: &Projection) {
    let specs = vec![
        // ---- BLUE ----
        UnitSpec {
            side: Side::Blue,
            kind: PlatformKind::Destroyer,
            name: "USS HALSEY",
            hull: "DDG 97",
            sensors: Sensors { radar_m: km(40.0), sonar_m: km(12.0), visual_m: 10_000.0 },
            weapons: vec![
                WeaponSpec { name: "SM-2 SAM", range_km: 90.0 },
                WeaponSpec { name: "VLA ASROC", range_km: 22.0 },
                WeaponSpec { name: "5in Gun", range_km: 24.0 },
            ],
            start: (21.345, -157.975),
            heading_deg: 200.0,
            patrol: vec![(21.345, -157.975), (21.328, -157.995), (21.352, -157.988)],
            patrol_loop: true,
        },
        UnitSpec {
            side: Side::Blue,
            kind: PlatformKind::MPatrol,
            name: "POSEIDON 41",
            hull: "P-8A",
            sensors: Sensors { radar_m: km(90.0), sonar_m: None, visual_m: 18_000.0 },
            weapons: vec![
                WeaponSpec { name: "Harpoon SSM", range_km: 130.0 },
                WeaponSpec { name: "MK 54 Torpedo", range_km: 10.0 },
            ],
            start: (21.32, -158.14),
            heading_deg: 120.0,
            patrol: vec![(21.32, -158.14), (21.21, -158.02)],
            patrol_loop: true,
        },
        UnitSpec {
            side: Side::Blue,
            kind: PlatformKind::Submarine,
            name: "USS TEXAS",
            hull: "SSN 775",
            sensors: Sensors { radar_m: None, sonar_m: km(18.0), visual_m: 0.0 },
            weapons: vec![WeaponSpec { name: "MK 48 ADCAP", range_km: 35.0 }],
            start: (21.30, -158.06),
            heading_deg: 60.0,
            patrol: vec![(21.30, -158.06), (21.27, -157.98)],
            patrol_loop: true,
        },
        UnitSpec {
            side: Side::Blue,
            kind: PlatformKind::Facility,
            name: "KOA POINT RADAR",
            hull: "SITE",
            sensors: Sensors { radar_m: km(70.0), sonar_m: None, visual_m: 12_000.0 },
            weapons: vec![],
            start: (21.365, -157.90),
            heading_deg: 0.0,
            patrol: vec![],
            patrol_loop: false,
        },
        UnitSpec {
            side: Side::Blue,
            kind: PlatformKind::Facility,
            name: "HICKAM FIELD",
            hull: "BASE",
            sensors: Sensors { radar_m: km(30.0), sonar_m: None, visual_m: 15_000.0 },
            weapons: vec![],
            start: (21.325, -157.91),
            heading_deg: 0.0,
            patrol: vec![],
            patrol_loop: false,
        },
        // ---- RED ----
        UnitSpec {
            side: Side::Red,
            kind: PlatformKind::Destroyer,
            name: "KRAKEN",
            hull: "CG 01",
            sensors: Sensors { radar_m: km(38.0), sonar_m: km(10.0), visual_m: 10_000.0 },
            weapons: vec![
                WeaponSpec { name: "Shipwreck SSM", range_km: 200.0 },
                WeaponSpec { name: "SA-N-6 SAM", range_km: 80.0 },
            ],
            start: (21.20, -158.30),
            heading_deg: 45.0,
            patrol: vec![(21.28, -158.12)],
            patrol_loop: false,
        },
        UnitSpec {
            side: Side::Red,
            kind: PlatformKind::Frigate,
            name: "MANTICORE",
            hull: "FFG 02",
            sensors: Sensors { radar_m: km(25.0), sonar_m: km(6.0), visual_m: 9_000.0 },
            weapons: vec![WeaponSpec { name: "Sizzler SSM", range_km: 100.0 }],
            start: (21.18, -158.26),
            heading_deg: 45.0,
            patrol: vec![(21.27, -158.08)],
            patrol_loop: false,
        },
        UnitSpec {
            side: Side::Red,
            kind: PlatformKind::Fighter,
            name: "FULCRUM 11",
            hull: "MIG-29K",
            sensors: Sensors { radar_m: km(60.0), sonar_m: None, visual_m: 12_000.0 },
            weapons: vec![WeaponSpec { name: "Archer AAM", range_km: 60.0 }],
            start: (21.16, -158.18),
            heading_deg: 90.0,
            patrol: vec![
                (21.16, -158.18),
                (21.16, -158.06),
                (21.24, -158.06),
                (21.24, -158.18),
            ],
            patrol_loop: true,
        },
        UnitSpec {
            side: Side::Red,
            kind: PlatformKind::Submarine,
            name: "LEVIATHAN",
            hull: "SSK 09",
            sensors: Sensors { radar_m: None, sonar_m: km(8.0), visual_m: 0.0 },
            weapons: vec![WeaponSpec { name: "Heavyweight Torpedo", range_km: 40.0 }],
            start: (21.12, -158.06),
            heading_deg: 45.0,
            patrol: vec![(21.26, -157.99)],
            patrol_loop: false,
        },
        // ---- NEUTRAL ----
        UnitSpec {
            side: Side::Neutral,
            kind: PlatformKind::Merchant,
            name: "MV PACIFIC TRADER",
            hull: "IMO 9xxxx",
            sensors: Sensors { radar_m: None, sonar_m: None, visual_m: 3_000.0 },
            weapons: vec![],
            start: (21.10, -158.25),
            heading_deg: 90.0,
            patrol: vec![(21.10, -157.75)],
            patrol_loop: false,
        },
    ];

    for s in specs {
        let pos = proj.project(s.start.0, s.start.1);
        // 罗盘航向（北 0 顺时针）→ 数学角
        let heading = (90.0 - s.heading_deg).to_radians();
        let patrol: Vec<Vec2> = s.patrol.iter().map(|&(lat, lon)| proj.project(lat, lon)).collect();
        let route = patrol.clone();
        let moving = !patrol.is_empty() && s.kind.mobile();
        commands.spawn((
            Unit {
                side: s.side,
                kind: s.kind,
                name: s.name.into(),
                hull: s.hull.into(),
                sensors: s.sensors,
                weapons: s.weapons,
                patrol: patrol.clone(),
                route,
                wp_index: 0,
                route_loop: s.patrol_loop,
                moving,
                detected_by_blue: s.side != Side::Red,
                classified: s.side != Side::Red,
                last_seen_t: if s.side != Side::Red { Some(0.0) } else { None },
            },
            Position(pos),
            Heading(heading),
            SpeedMps(s.kind.cruise_kts() * KTS_TO_MPS),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(route: Vec<Vec2>, route_loop: bool) -> Unit {
        Unit {
            side: Side::Blue,
            kind: PlatformKind::Destroyer,
            name: "T".into(),
            hull: "T".into(),
            sensors: Sensors::default(),
            weapons: vec![],
            patrol: route.clone(),
            route,
            wp_index: 0,
            route_loop,
            moving: true,
            detected_by_blue: true,
            classified: true,
            last_seen_t: Some(0.0),
        }
    }

    #[test]
    fn steer_turn_limits() {
        // 目标在正后方（差 180 度），单次只能转 max_turn
        let h = steer_turn(0.0, std::f32::consts::PI, 0.1);
        assert!((h - 0.1).abs() < 1e-5, "h = {h}");
        // 小角度直接到达
        let h2 = steer_turn(0.0, 0.05, 0.1);
        assert!((h2 - 0.05).abs() < 1e-5);
        // 跨 ±PI 边界选最短方向
        let h3 = steer_turn(3.10, -3.10, 1.0);
        assert!(h3 > 3.10, "应向 +方向 转过 PI 边界, h3 = {h3}");
    }

    #[test]
    fn step_unit_moves_and_arrives() {
        let mut u = unit(vec![Vec2::new(1000.0, 0.0)], false);
        let mut pos = Vec2::ZERO;
        let mut heading = 0.0f32; // 正东
        let mut speed = 10.0f32;
        // 10 m/s 走 50s → 500m，未到
        let arrived = step_unit(&mut u, &mut pos, &mut heading, &mut speed, 50.0);
        assert!(!arrived);
        assert!((pos.x - 500.0).abs() < 1.0, "pos = {pos:?}");
        // 再走 50s+ 到达
        let arrived2 = step_unit(&mut u, &mut pos, &mut heading, &mut speed, 60.0);
        assert!(arrived2, "应在 1000m 处到达, pos = {pos:?}");
    }

    #[test]
    fn waypoint_advance_modes() {
        // 单程：终点到达后停车
        let mut u = unit(vec![Vec2::new(100.0, 0.0), Vec2::new(200.0, 0.0)], false);
        u.wp_index = 1;
        advance_waypoint(&mut u);
        assert!(!u.moving, "单程航线走完应停车");
        // 循环：回卷
        let mut u2 = unit(vec![Vec2::new(100.0, 0.0), Vec2::new(200.0, 0.0)], true);
        u2.wp_index = 1;
        advance_waypoint(&mut u2);
        assert_eq!(u2.wp_index, 0);
        assert!(u2.moving);
    }

    #[test]
    fn order_move_and_resume_patrol() {
        let mut u = unit(vec![Vec2::new(100.0, 0.0)], true);
        u.order_move(Vec2::new(-500.0, -500.0));
        assert_eq!(u.route, vec![Vec2::new(-500.0, -500.0)]);
        assert!(!u.route_loop);
        u.resume_patrol();
        assert_eq!(u.route, vec![Vec2::new(100.0, 0.0)]);
        assert!(u.route_loop);
    }

    #[test]
    fn detection_domains_and_ranges() {
        let ship_radar = Sensors { radar_m: Some(40_000.0), sonar_m: Some(12_000.0), visual_m: 10_000.0 };
        // 雷达 35km 处发现水面目标，未分类
        assert_eq!(detect_with(&ship_radar, Domain::Surface, 35_000.0), DetectResult::Detected { classified: false });
        // 15km 处分类
        assert_eq!(detect_with(&ship_radar, Domain::Surface, 15_000.0), DetectResult::Detected { classified: true });
        // 雷达/目视都看不到潜艇，只有声纳可以
        assert_eq!(detect_with(&ship_radar, Domain::Subsurface, 35_000.0), DetectResult::None);
        assert_eq!(detect_with(&ship_radar, Domain::Subsurface, 11_000.0), DetectResult::Detected { classified: false });
        // 超出所有传感器
        assert_eq!(detect_with(&ship_radar, Domain::Air, 90_000.0), DetectResult::None);
        // 潜艇浮标/目视为零，对空无探测
        let sub = Sensors { radar_m: None, sonar_m: Some(18_000.0), visual_m: 0.0 };
        assert_eq!(detect_with(&sub, Domain::Air, 5_000.0), DetectResult::None);
    }

    #[test]
    fn clock_format_and_speed() {
        let mut c = SimClock::default();
        assert_eq!(c.format(), "D+0 04:00:00Z");
        c.t = 3600.0 * 20.0; // 20 小时后 → 次日 00:00
        assert_eq!(c.format(), "D+1 00:00:00Z");
        c.speed_idx = 2;
        assert_eq!(c.multiplier(), 30.0);
        assert_eq!(c.sim_dt(2.0), 60.0);
        c.paused = true;
        assert_eq!(c.sim_dt(2.0), 0.0);
    }
}
