//! Cesium 风格时间轴：底部全宽刻度轴 + 可拖动时间把手 + 滚轮缩放窗口。
//!
//! - 窗口 [start_unix, start_unix+span]，播放时自动前滚（当前时间触达右缘 90%）；
//! - 拖把手/点击空白改场景时间（拖动期间自动暂停）；
//! - 滚轮在轴区缩放跨度（2 分钟 ~ 14 天）；
//! - 刻度间隔自适应（1m/5m/15m/1h/3h/6h/12h/1d/3d/7d），major 带标签、
//!   跨日处显示日期；
//! - 纯函数（窗口/刻度/映射/格式化）有单测，UI 每 0.2s 或窗口变化时重建刻度。

use bevy::ecs::entity::Entity;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Local, Query, Res, ResMut};
use bevy::input::mouse::MouseButton;
use bevy::math::Rect;
use bevy::prelude::*;
use bevy::time::Time;
use bevy::ui::{BackgroundColor, Val};
use bevy::window::{PrimaryWindow, Window};

use crate::camera::CursorState;
use crate::input::UiHitZones;
use crate::map_render::palette;
use crate::satellites::SCENARIO_EPOCH_UNIX;
use crate::sim::SimClock;

const BAR_H: f32 = 56.0;
/// 轴区左留白（当前时刻文本）
const PAD_L: f32 = 150.0;
const PAD_R: f32 = 12.0;
/// 缩放范围
pub const MIN_SPAN: f64 = 120.0;
pub const MAX_SPAN: f64 = 14.0 * 86_400.0;

// ---------- 纯函数 ----------

/// 自适应刻度间隔候选（秒）
const STEP_CANDIDATES: [(f64, &str); 12] = [
    (60.0, "1m"),
    (300.0, "5m"),
    (900.0, "15m"),
    (1800.0, "30m"),
    (3600.0, "1h"),
    (3.0 * 3600.0, "3h"),
    (6.0 * 3600.0, "6h"),
    (12.0 * 3600.0, "12h"),
    (86_400.0, "1d"),
    (3.0 * 86_400.0, "3d"),
    (7.0 * 86_400.0, "7d"),
    (30.0 * 86_400.0, "30d"),
];

/// 选择刻度间隔：轴宽 px 下目标 major 间距 ≥ 90px
pub fn step_for_span(span: f64, axis_w: f32) -> f64 {
    let px_per_sec = axis_w as f64 / span;
    for (s, _) in STEP_CANDIDATES {
        if s * px_per_sec >= 90.0 {
            return s;
        }
    }
    STEP_CANDIDATES[STEP_CANDIDATES.len() - 1].0
}

/// 一条刻度
#[derive(Debug, Clone, PartialEq)]
pub struct Tick {
    pub unix: f64,
    pub major: bool,
    pub label: Option<String>,
}

/// 生成窗口内刻度（首尾略外扩半步，label：major=hh:mm，跨日=date_label）
pub fn ticks_for_window(start: f64, span: f64, axis_w: f32) -> Vec<Tick> {
    let step = step_for_span(span, axis_w);
    let minor = step / 5.0;
    let end = start + span;
    let mut out = Vec::new();
    let mut u = (start / minor).floor() * minor;
    while u <= end + minor {
        let aligned = ((u / step).round() * step - u).abs() < minor * 0.25;
        let unix = u;
        let label = if aligned {
            let civil = civil_from_unix(unix);
            // 跨日：00:00 附近的 major 显示日期
            if (civil.hour as f64 + civil.min as f64 / 60.0) * 3600.0 < step {
                Some(date_label(&civil))
            } else {
                Some(format!("{:02}:{:02}", civil.hour, civil.min))
            }
        } else {
            None
        };
        out.push(Tick { unix, major: aligned, label });
        u += minor;
    }
    out
}

pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

/// Unix 秒 → UTC 民用时（Hinnant 算法，无 chrono 依赖）
pub fn civil_from_unix(unix: f64) -> Civil {
    let secs = unix.floor() as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    Civil {
        year: y,
        month: m,
        day: d as u32,
        hour: (sod / 3600) as u32,
        min: ((sod % 3600) / 60) as u32,
        sec: (sod % 60) as u32,
    }
}

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

pub fn date_label(c: &Civil) -> String {
    format!("{} {}", MONTHS[(c.month - 1) as usize], c.day)
}

/// unix ↔ 轴像素
pub fn unix_to_x(unix: f64, start: f64, span: f64, axis_w: f32) -> f32 {
    ((unix - start) / span) as f32 * axis_w
}
pub fn x_to_unix(x: f32, start: f64, span: f64, axis_w: f32) -> f64 {
    start + x as f64 / axis_w as f64 * span
}

/// 播放时窗口前滚：当前时间越过 90% 处
pub fn roll_window(start: &mut f64, span: f64, now_unix: f64) {
    let end = *start + span;
    if now_unix > *start + span * 0.9 || now_unix < *start {
        *start = now_unix - span * 0.1;
    }
    let _ = end;
}

// ---------- 资源与组件 ----------

#[derive(Resource)]
pub struct TimeWindow {
    pub start: f64,
    pub span: f64,
}

impl Default for TimeWindow {
    fn default() -> Self {
        // 初始窗口：场景起点前 10% ~ 后 110%（跨 ~3h）
        TimeWindow { start: SCENARIO_EPOCH_UNIX - 600.0, span: 3.0 * 3600.0 }
    }
}

#[derive(Component)]
struct TickMark;
#[derive(Component)]
struct TickLabel;
#[derive(Component)]
struct Scrubber;
#[derive(Component)]
struct ElapsedBar;

#[derive(Resource)]
pub struct TimelineNodes {
    #[allow(dead_code)] // 预留：整条显隐
    bar_root: Entity,
    now_text: Entity,
    date_text: Entity,
    scrubber: Entity,
    elapsed: Entity,
    /// 刻度实体池（按需重建）
    ticks: Vec<Entity>,
}

#[derive(Default)]
pub(crate) struct ScrubDrag {
    active: bool,
}

// ---------- UI 构建 ----------

pub fn build_timeline(mut commands: Commands) {
    let bar = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                bottom: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Px(BAR_H),
                ..default()
            },
            BackgroundColor(Color::srgba(0.043, 0.055, 0.07, 0.96)),
        ))
        .id();
    let now_text = commands
        .spawn((
            Text::new("--"), TextColor(Color::WHITE),
            TextFont::from_font_size(16.0),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(10.0),
                top: Val::Px(8.0),
                ..default()
            },
        ))
        .id();
    let date_text = commands
        .spawn((
            Text::new(""), TextColor(Color::srgb_u8(148, 160, 172)),
            TextFont::from_font_size(11.0),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(10.0),
                top: Val::Px(30.0),
                ..default()
            },
        ))
        .id();
    // 轴基线（灰，右侧暗段）
    let elapsed = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(PAD_L),
                top: Val::Px(36.0),
                width: Val::Px(200.0),
                height: Val::Px(3.0),
                ..default()
            },
            BackgroundColor(palette::SIDE_BLUE.with_alpha(0.9)),
            ElapsedBar,
        ))
        .id();
    let scrubber = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(PAD_L),
                top: Val::Px(14.0),
                width: Val::Px(9.0),
                height: Val::Px(28.0),
                ..default()
            },
            BackgroundColor(palette::SELECT),
            Scrubber,
        ))
        .id();
    // 轴线（暗色全长，在 elapsed 下层——用单独实体）
    let hint = commands
        .spawn((
            Text::new("drag: scrub  wheel: zoom span  click: jump"),
            TextColor(Color::srgba(0.58, 0.63, 0.68, 0.5)),
            TextFont::from_font_size(9.0),
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(16.0),
                top: Val::Px(4.0),
                ..default()
            },
        ))
        .id();
    let _ = (bar, hint);
    commands.insert_resource(TimelineNodes {
        bar_root: bar,
        now_text,
        date_text,
        scrubber,
        elapsed,
        ticks: Vec::new(),
    });
}

// ---------- 刷新 ----------

#[allow(clippy::too_many_arguments)]
pub fn timeline_system(
    mut commands: Commands,
    mut clock: ResMut<SimClock>,
    mut window_res: ResMut<TimeWindow>,
    nodes: Option<ResMut<TimelineNodes>>,
    cursor: Res<CursorState>,
    mouse: Res<ButtonInput<MouseButton>>,
    scroll: Res<bevy::input::mouse::AccumulatedMouseScroll>,
    win: Query<&Window, With<PrimaryWindow>>,
    time: Res<Time>,
    mut texts: Query<&mut Text>,
    mut nodes_q: Query<&mut Node>,
    mut drag: Local<ScrubDrag>,
    mut rebuild_at: Local<f32>,
) {
    let Some(mut n) = nodes else { return };
    let now_unix = SCENARIO_EPOCH_UNIX + clock.t;

    // ---- 交互（先于窗口滚动，使拖动生效） ----
    let (w, h) = win.single().map(|w| (w.width(), w.height())).unwrap_or((1600.0, 900.0));
    let axis_w = w - PAD_L - PAD_R;
    let in_bar = cursor
        .screen
        .map(|c| c.y >= h - BAR_H && c.y <= h)
        .unwrap_or(false);
    let ax = cursor.screen.map(|c| c.x - PAD_L);

    // 滚轮缩放窗口跨度（以当前时间为锚）
    if in_bar && scroll.delta.y.abs() > 1e-4 {
        let f = if scroll.delta.y > 0.0 { 1.0 / 1.25 } else { 1.25 };
        window_res.span = (window_res.span * f).clamp(MIN_SPAN, MAX_SPAN);
        window_res.start = now_unix - window_res.span * 0.1;
        *rebuild_at = 0.0;
    }

    if in_bar {
        if mouse.pressed(MouseButton::Left) {
            if !drag.active {
                drag.active = true;
                // 点击即跳时间（含把手拖动开始）
                if let Some(x) = ax {
                    let (start, span) = (window_res.start, window_res.span);
                    let target = x_to_unix(x.clamp(0.0, axis_w), start, span, axis_w);
                    clock.t = (target - SCENARIO_EPOCH_UNIX).max(0.0);
                    roll_window(&mut window_res.start, span, target);
                }
            } else if let Some(x) = ax {
                let (start, span) = (window_res.start, window_res.span);
                let target = x_to_unix(x.clamp(0.0, axis_w), start, span, axis_w);
                clock.t = (target - SCENARIO_EPOCH_UNIX).max(0.0);
                roll_window(&mut window_res.start, span, target);
            }
        } else {
            drag.active = false;
        }
    } else {
        drag.active = false;
    }

    // ---- 播放时窗口跟随 ----
    if !clock.paused && !drag.active {
        let span = window_res.span;
        roll_window(&mut window_res.start, span, now_unix);
    }

    // ---- 文本/把手位置（每帧，低频节流） ----
    *rebuild_at += time.delta().as_secs_f32();
    let dirty = *rebuild_at > 0.2;
    if dirty {
        *rebuild_at = 0.0;
        let civil = civil_from_unix(now_unix);
        let day = ((now_unix - SCENARIO_EPOCH_UNIX) / 86_400.0).floor() as i64;
        if let Ok(mut t) = texts.get_mut(n.now_text) {
            t.0 = format!("D+{} {:02}:{:02}:{:02}Z", day, civil.hour, civil.min, civil.sec);
        }
        if let Ok(mut t) = texts.get_mut(n.date_text) {
            t.0 = format!("{:04}-{:02}-{:02}", civil.year, civil.month, civil.day);
        }
        // 刻度重建
        for e in n.ticks.drain(..) {
            commands.entity(e).despawn();
        }
        let ticks = ticks_for_window(window_res.start, window_res.span, axis_w);
        let mut tick_ents = Vec::new();
        for tk in &ticks {
            let x = unix_to_x(tk.unix, window_res.start, window_res.span, axis_w);
            if x < -20.0 || x > axis_w + 20.0 {
                continue;
            }
            let e = commands
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(PAD_L + x - 0.5),
                        bottom: Val::Px(if tk.major { 8.0 } else { 14.0 }),
                        width: Val::Px(if tk.major { 2.0 } else { 1.0 }),
                        height: Val::Px(if tk.major { 14.0 } else { 8.0 }),
                        ..default()
                    },
                    BackgroundColor(if tk.major {
                        Color::srgba(0.6, 0.65, 0.7, 0.9)
                    } else {
                        Color::srgba(0.6, 0.65, 0.7, 0.4)
                    }),
                    TickMark,
                ))
                .id();
            tick_ents.push(e);
            if let Some(label) = &tk.label {
                let le = commands
                    .spawn((
                        Text::new(label.clone()),
                        TextColor(Color::srgb_u8(148, 160, 172)),
                        TextFont::from_font_size(10.0),
                        Node {
                            position_type: PositionType::Absolute,
                            left: Val::Px(PAD_L + x - 20.0),
                            bottom: Val::Px(24.0),
                            width: Val::Px(40.0),
                            ..default()
                        },
                        TickLabel,
                    ))
                    .id();
                tick_ents.push(le);
            }
        }
        n.ticks = tick_ents;
    }

    // ---- 把手/已过条（每帧，便宜） ----
    let now_x = unix_to_x(now_unix, window_res.start, window_res.span, axis_w).clamp(0.0, axis_w);
    let (scrub_e, elapsed_e) = (n.scrubber, n.elapsed);
    if let Ok(mut node) = nodes_q.get_mut(scrub_e) {
        node.left = Val::Px(PAD_L + now_x - 4.5);
    }
    if let Ok(mut node) = nodes_q.get_mut(elapsed_e) {
        node.left = Val::Px(PAD_L);
        node.width = Val::Px(now_x.max(1.0));
    }
}

/// 把时间轴区域追加进 UI 命中区（供全局交互屏蔽）
pub fn timeline_hit_zone(
    win: Query<&Window, With<PrimaryWindow>>,
    mut zones: ResMut<UiHitZones>,
) {
    let Ok(w) = win.single() else { return };
    zones.zones.push((Rect::new(0.0, w.height() - BAR_H, w.width(), w.height()), "timeline"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_selection() {
        // 3h 跨 800px → px/s=0.074 → 1h 间隔 267px ≥90 但 30m=133 也 ≥90 → 选 30m
        assert!((step_for_span(3.0 * 3600.0, 800.0) - 1800.0).abs() < 1.0);
        // 7 天 → 1d
        assert!((step_for_span(7.0 * 86_400.0, 800.0) - 86_400.0).abs() < 1.0);
        // 2 分钟 → 1m
        assert!((step_for_span(120.0, 800.0) - 60.0).abs() < 1.0);
    }

    #[test]
    fn ticks_align_and_labels() {
        let start = SCENARIO_EPOCH_UNIX; // 04:00Z
        let tks = ticks_for_window(start, 3600.0, 800.0);
        assert!(tks.len() > 8 && tks.len() < 40, "刻度数 {}", tks.len());
        // major 刻度对齐 5 分钟
        for tk in tks.iter().filter(|t| t.major) {
            assert!((tk.unix % 300.0).abs() < 1e-3, "major 未对齐: {}", tk.unix);
            assert!(tk.label.is_some());
        }
        // 首个 major 标签是 04:00 或 04:05
        let first = tks.iter().find(|t| t.major).unwrap();
        assert!(first.label.as_deref().unwrap().contains(':'));
    }

    #[test]
    fn ticks_day_boundary_label() {
        // 跨日窗口（23:30 ~ 00:30）→ 00:00 major 显示日期（SEP/…）
        let start = SCENARIO_EPOCH_UNIX + 20.0 * 3600.0; // 次日 00:00
        let tks = ticks_for_window(start - 3600.0, 7200.0, 800.0);
        let mid = tks
            .iter()
            .find(|t| t.major && (t.unix - start).abs() < 60.0);
        assert!(mid.is_some());
        assert!(mid.unwrap().label.as_deref().unwrap().contains(' '), "跨日应显示日期: {:?}", mid.unwrap().label);
    }

    #[test]
    fn unix_x_roundtrip() {
        let (start, span, w) = (1000.0, 3600.0, 800.0f32);
        let x = unix_to_x(2800.0, start, span, w);
        assert!((x - 400.0).abs() < 1e-3);
        let u = x_to_unix(x, start, span, w);
        assert!((u - 2800.0).abs() < 1e-6);
    }

    #[test]
    fn roll_window_follows() {
        let mut start = 0.0;
        roll_window(&mut start, 3600.0, 3500.0); // 越过 90%
        assert!((start - 3500.0 + 360.0).abs() < 1e-6, "start={start}");
        // 回看：时间跳到窗口前 → 窗口重定位
        let mut s2 = 10_000.0;
        roll_window(&mut s2, 3600.0, 5_000.0);
        assert!((s2 - 4640.0).abs() < 1e-6);
    }

    #[test]
    fn civil_conversion() {
        let c = civil_from_unix(SCENARIO_EPOCH_UNIX);
        assert_eq!((c.year, c.month, c.day), (2026, 9, 12));
        assert_eq!((c.hour, c.min, c.sec), (4, 0, 0));
        let leap = civil_from_unix(951_782_400.0); // 2000-02-29
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));
    }
}
