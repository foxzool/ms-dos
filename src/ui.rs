//! HUD：顶栏（标题/时钟/倍速按钮）、接触列表、所选单位信息面板。
//!
//! 按钮与面板用固定/锚定的像素布局，便于手动命中测试（不依赖 picking 后端）；
//! 每帧刷新的文本按 4Hz 节流。

use bevy::prelude::*;
use bevy::state::state::State;
use bevy::window::PrimaryWindow;

use crate::camera::{CameraRig, CursorState};
use crate::globe::AppState;
use crate::input::{range_bearing_text, Selection, UiHitZones};
use crate::map_render::palette;
use crate::sim::{Heading, KTS_TO_MPS, Position, Side, SimClock, SpeedMps, Unit, TIME_SPEEDS};

use crate::MapCtx;

const BAR_H: f32 = 44.0;
const BTN_Y: f32 = 7.0;
const BTN_W: f32 = 66.0;
const BTN_H: f32 = 30.0;
const BTN_GAP: f32 = 6.0;
const BTN_X0: f32 = 330.0;
const CONTACT_W: f32 = 256.0;
const CONTACT_H: f32 = 330.0;
const PANEL_W: f32 = 396.0;
const PANEL_H: f32 = 176.0;
/// 底部时间轴高度（避让）
const TIMELINE_H: f32 = 56.0;
const ROWS: usize = 14;

#[derive(Clone, Copy, PartialEq)]
enum BtnAction {
    Pause,
    Speed(usize),
}

#[derive(Resource)]
pub struct UiNodes {
    clock_text: Entity,
    contact_rows: Vec<Entity>,
    panel_lines: Vec<Entity>,
    buttons: Vec<(Entity, BtnAction, Rect)>,
}

fn panel_bg(alpha: f32) -> BackgroundColor {
    BackgroundColor(Color::srgba(0.043, 0.055, 0.07, alpha))
}

fn text_node(text: &str, size: f32, color: Color) -> (Text, TextColor, TextFont) {
    (Text::new(text), TextColor(color), TextFont::from_font_size(size))
}

const DIM: Color = Color::srgb_u8(148, 160, 172);

pub fn build_ui(mut commands: Commands) {
    // ---- 顶栏 ----
    let bar = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(0.0),
                left: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Px(BAR_H),
                ..Default::default()
            },
            panel_bg(0.94),
        ))
        .id();

    let button_specs: Vec<(&str, BtnAction)> = vec![
        ("PAUSE", BtnAction::Pause),
        ("1x", BtnAction::Speed(0)),
        ("5x", BtnAction::Speed(1)),
        ("30x", BtnAction::Speed(2)),
        ("120x", BtnAction::Speed(3)),
        ("600x", BtnAction::Speed(4)),
    ];
    let mut button_ids: Vec<(Entity, BtnAction)> = Vec::new();
    commands.entity(bar).with_children(|p| {
        p.spawn((
            text_node("MIAOSUAN DOS", 15.0, palette::SIDE_BLUE),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(10.0),
                top: Val::Px(12.0),
                ..Default::default()
            },
        ));
        p.spawn((
            text_node("PEARL HARBOR WATCH", 12.0, DIM),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(160.0),
                top: Val::Px(15.0),
                ..Default::default()
            },
        ));
        for (i, (label, action)) in button_specs.iter().enumerate() {
            let x = BTN_X0 + i as f32 * (BTN_W + BTN_GAP);
            let e = p
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(x),
                        top: Val::Px(BTN_Y),
                        width: Val::Px(BTN_W),
                        height: Val::Px(BTN_H),
                        ..Default::default()
                    },
                    BackgroundColor(Color::srgb_u8(28, 35, 44)),
                ))
                .with_child(text_node(label, 12.0, Color::srgb_u8(200, 210, 220)))
                .id();
            button_ids.push((e, *action));
        }
    });

    let clock_text = commands
        .spawn((
            text_node("--", 14.0, Color::WHITE),
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(12.0),
                top: Val::Px(12.0),
                ..Default::default()
            },
        ))
        .id();

    // ---- 接触列表面板 ----
    let contacts = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(8.0),
                top: Val::Px(BAR_H + 8.0),
                width: Val::Px(CONTACT_W),
                height: Val::Px(CONTACT_H),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(8.0)),
                ..Default::default()
            },
            panel_bg(0.88),
        ))
        .id();
    let mut contact_rows = Vec::new();
    commands.entity(contacts).with_children(|p| {
        p.spawn((
            text_node("CONTACTS", 12.0, palette::SELECT),
            Node { margin: UiRect::bottom(Val::Px(6.0)), ..Default::default() },
        ));
        for _ in 0..ROWS {
            contact_rows.push(
                p.spawn((
                    text_node("", 12.0, DIM),
                    Node { margin: UiRect::px(0.0, 0.0, 1.5, 1.5), ..Default::default() },
                ))
                .id(),
            );
        }
    });

    // ---- 单位信息面板 ----
    let panel = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(8.0),
                bottom: Val::Px(TIMELINE_H + 8.0),
                width: Val::Px(PANEL_W),
                height: Val::Px(PANEL_H),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(8.0)),
                ..Default::default()
            },
            panel_bg(0.88),
        ))
        .id();
    let mut panel_lines = Vec::new();
    commands.entity(panel).with_children(|p| {
        for _ in 0..7 {
            panel_lines.push(p.spawn((text_node("", 12.0, DIM), Node::default())).id());
        }
    });

    // 地图数据署名（ODbL 与 OSM 瓦片使用准则要求）
    commands.spawn((
        text_node("(c) OpenStreetMap contributors", 10.0, Color::srgba(0.58, 0.63, 0.68, 0.85)),
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(10.0),
            bottom: Val::Px(6.0),
            ..Default::default()
        },
    ));

    commands.insert_resource(UiNodes {
        clock_text,
        contact_rows,
        panel_lines,
        buttons: button_ids
            .into_iter()
            .enumerate()
            .map(|(i, (e, a))| {
                let x = BTN_X0 + i as f32 * (BTN_W + BTN_GAP);
                (e, a, Rect::new(x, BTN_Y, x + BTN_W, BTN_Y + BTN_H))
            })
            .collect(),
    });
}

/// 顶栏按钮点击（即时处理）
fn ui_button_clicks(
    cursor: Res<CursorState>,
    mouse: Res<ButtonInput<MouseButton>>,
    ui: Res<UiNodes>,
    mut clock: ResMut<SimClock>,
) {
    if mouse.just_released(MouseButton::Left) {
        if let Some(c) = cursor.screen {
            for (_, action, r) in &ui.buttons {
                if r.contains(c) {
                    match action {
                        BtnAction::Pause => clock.paused = !clock.paused,
                        BtnAction::Speed(idx) => {
                            clock.speed_idx = *idx;
                            clock.paused = false;
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn update_ui(
    time: Res<Time>,
    mut throttle: Local<f32>,
    clock: ResMut<SimClock>,
    selection: Res<Selection>,
    units: Query<(Entity, &Unit, &Position, &Heading, &SpeedMps)>,
    rig: Res<CameraRig>,
    window: Query<&Window, With<PrimaryWindow>>,
    ctx: Res<MapCtx>,
    ui: Res<UiNodes>,
    mut texts: Query<&mut Text>,
    mut colors: Query<&mut TextColor>,
    mut btn_bg: Query<&mut BackgroundColor>,
    mut zones: ResMut<UiHitZones>,
    app_state: Res<State<AppState>>,
    tiles: Option<Res<crate::tiles::TileCache>>,
    sat_layer: Option<Res<crate::satellites::SatLayer>>,
) {
    // ---- 按钮点击（即时处理，不节流） ----
    let (win_w, win_h) = window
        .single()
        .map(|w| (w.width(), w.height()))
        .unwrap_or((1280.0, 800.0));

    // ---- 4Hz 节流刷新 ----
    *throttle += time.delta().as_secs_f32();
    if *throttle < 0.25 {
        return;
    }
    *throttle = 0.0;

    // 时钟
    if let Ok(mut t) = texts.get_mut(ui.clock_text) {
        let speed = if clock.paused {
            "PAUSED".to_string()
        } else {
            format!("x{}", TIME_SPEEDS[clock.speed_idx])
        };
        let mode_tag = if *app_state.get() == AppState::Globe { "GLOBE" } else { "MAP" };
        let sat_tag = if *app_state.get() == AppState::Globe {
            match sat_layer {
                Some(l) => format!("  SAT {} {}", if l.visible { "ON" } else { "OFF" }, l.sat_count()),
                None => String::new(),
            }
        } else {
            String::new()
        };
        t.0 = format!("{}  {}  {}{}", clock.format(), speed, mode_tag, sat_tag);
    }

    // 按钮高亮
    for (e, action, _) in &ui.buttons {
        let active = match action {
            BtnAction::Pause => clock.paused,
            BtnAction::Speed(i) => !clock.paused && clock.speed_idx == *i,
        };
        if let Ok(mut bg) = btn_bg.get_mut(*e) {
            bg.0 = if active { Color::srgb_u8(46, 74, 102) } else { Color::srgb_u8(28, 35, 44) };
        }
    }

    // 参考点：所选单位或视野中心
    let origin = selection
        .0
        .and_then(|s| units.get(s).ok().map(|(_, _, p, _, _)| p.0))
        .unwrap_or(rig.target);

    // 接触列表：红方已探测 + 中立，按距离排序
    let mut contacts: Vec<(f32, &Unit, Vec2)> = units
        .iter()
        .filter(|(_, u, _, _, _)| (u.side == Side::Red && u.detected_by_blue) || u.side == Side::Neutral)
        .map(|(_, u, p, _, _)| (origin.distance(p.0), u, p.0))
        .collect();
    contacts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    for (i, row_e) in ui.contact_rows.iter().enumerate() {
        let Ok(mut t) = texts.get_mut(*row_e) else { continue };
        let Ok(mut c) = colors.get_mut(*row_e) else { continue };
        if let Some((_, u, pos)) = contacts.get(i) {
            let unknown = u.side == Side::Red && !u.classified;
            let name = if unknown { "UNKNOWN" } else { u.kind.short() };
            t.0 = format!("{name:<8} {}", range_bearing_text(origin, *pos));
            c.0 = if u.side == Side::Neutral {
                palette::SIDE_NEUTRAL
            } else if unknown {
                palette::SELECT
            } else {
                palette::SIDE_RED
            };
        } else {
            t.0 = String::new();
        }
    }

    // 单位面板
    let sel = selection.0.and_then(|s| units.get(s).ok());
    for (i, line_e) in ui.panel_lines.iter().enumerate() {
        let Ok(mut t) = texts.get_mut(*line_e) else { continue };
        let Ok(mut c) = colors.get_mut(*line_e) else { continue };
        let Some((_, u, p, h, s)) = sel else {
            if *app_state.get() == AppState::Globe {
                t.0 = match i {
                    0 => "GLOBAL SITUATION".to_string(),
                    2 => "drag rotate | wheel dive in | right-click land".to_string(),
                    3 => "dive near the yellow ring (OSM data area)".to_string(),
                    _ => String::new(),
                };
                c.0 = if i == 0 { palette::SELECT } else { DIM };
            } else {
                t.0 = match i {
                    0 => "No unit selected".to_string(),
                    2 => match &tiles {
                        Some(cache) if cache.inflight > 0 => {
                            format!("FETCHING OSM TILES ({} in flight)...", cache.inflight)
                        }
                        Some(cache) if cache.failed > 0 => {
                            format!("OSM TILES: {} loaded, {} failed", cache.loaded_count(), cache.failed)
                        }
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                c.0 = DIM;
            }
            continue;
        };
        let unknown = u.side == Side::Red && !u.classified;
        let side_name = match u.side {
            Side::Blue => "BLUE",
            Side::Red => "RED",
            Side::Neutral => "NEUTRAL",
        };
        match i {
            0 => {
                t.0 = if unknown {
                    "UNKNOWN CONTACT".to_string()
                } else {
                    format!("{} {}", u.hull, u.name)
                };
                c.0 = match u.side {
                    Side::Neutral => palette::SIDE_NEUTRAL,
                    Side::Red => palette::SIDE_RED,
                    Side::Blue => palette::SIDE_BLUE,
                };
            }
            1 => {
                t.0 = if unknown {
                    "Classification pending".into()
                } else {
                    format!("{} | {}", u.kind.class_name(), side_name)
                };
                c.0 = DIM;
            }
            2 => {
                let (lat, lon) = ctx.proj.unproject(p.0);
                let hdg = ((90.0 - h.0.to_degrees()) % 360.0 + 360.0) % 360.0;
                t.0 = format!(
                    "POS {:.4} {:.4}   SPD {:.1}kt   HDG {:03.0}",
                    lat,
                    lon,
                    s.0 / KTS_TO_MPS,
                    hdg
                );
                c.0 = Color::WHITE;
            }
            3 => {
                let radar =
                    u.sensors.radar_m.map(|r| format!("RDR {:.0}km", r / 1000.0)).unwrap_or_default();
                let sonar =
                    u.sensors.sonar_m.map(|r| format!("SONAR {:.0}km", r / 1000.0)).unwrap_or_default();
                let vis = if u.sensors.visual_m > 0.0 {
                    format!("VIS {:.0}km", u.sensors.visual_m / 1000.0)
                } else {
                    String::new()
                };
                t.0 = format!("{radar}  {sonar}  {vis}");
                c.0 = DIM;
            }
            4 => {
                if u.weapons.is_empty() {
                    t.0 = "WPN: none".into();
                } else {
                    let w: Vec<String> =
                        u.weapons.iter().map(|w| format!("{} {:.0}km", w.name, w.range_km)).collect();
                    t.0 = format!("WPN: {}", w.join(" / "));
                }
                c.0 = DIM;
            }
            5 => {
                let mode = if !u.kind.mobile() {
                    "STATION"
                } else if !u.moving {
                    "HOLDING"
                } else if u.route_loop {
                    "PATROL"
                } else {
                    "TRANSIT"
                };
                let wp = u.route.get(u.wp_index).copied();
                let wp_txt = wp.map(|w| range_bearing_text(p.0, w)).unwrap_or_default();
                t.0 = format!(
                    "NAV: {mode}  wp {}/{}  {}",
                    u.wp_index + 1,
                    u.route.len().max(1),
                    wp_txt
                );
                c.0 = DIM;
            }
            _ => {
                t.0 = if u.side == Side::Blue {
                    "L-click select | R-click order move | F focus | R resume patrol".into()
                } else if let Some(seen) = u.last_seen_t {
                    let c2 = SimClock { t: seen, ..Default::default() };
                    format!("Last seen: {}", c2.format())
                } else {
                    String::new()
                };
                c.0 = DIM;
            }
        }
    }

    // ---- UI 命中区（供 input.rs 屏蔽地图交互） ----
    zones.zones = vec![
        (Rect::new(0.0, 0.0, win_w, BAR_H), "topbar"),
        (
            Rect::new(win_w - CONTACT_W - 16.0, BAR_H + 8.0, win_w, BAR_H + 8.0 + CONTACT_H),
            "contacts",
        ),
        (Rect::new(0.0, win_h - PANEL_H - 16.0 - TIMELINE_H, PANEL_W + 16.0, win_h - TIMELINE_H), "unitpanel"),
    ];
}

pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, build_ui)
            .add_systems(Update, (ui_button_clicks, update_ui).chain());
    }
}
