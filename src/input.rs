//! 交互：点击选择、右键机动下令、左键/中键拖拽平移、快捷键。
//!
//! UI 面板区域由 ui.rs 注册的 `UiHitZones` 提供命中测试，
//! 命中 UI 的指针事件不会穿透到地图。

use bevy::prelude::*;
use bevy::state::condition::in_state;
use bevy::state::state::{NextState, State};
use bevy::window::PrimaryWindow;

use crate::camera::{focus_on, CameraRig, CursorState};
use crate::geo::bearing_deg;
use crate::globe::{distance_for_mpp, AppState, GlobeRig, MAP_MAX_MPP};
use crate::sim::{Position, Side, SimClock, Unit, TIME_SPEEDS};
use crate::{MapCtx, RenderMode};

#[derive(Resource, Default)]
pub struct Selection(pub Option<Entity>);

/// UI 命中区（屏幕像素，原点左上）
#[derive(Resource, Default)]
pub struct UiHitZones {
    pub zones: Vec<(Rect, &'static str)>,
}

/// 左键拖拽状态机：按下未移动 = 点击，移动超阈值 = 平移
#[derive(Resource, Default)]
struct DragState {
    down: bool,
    last: Vec2,
    moved: f32,
}

/// 中键拖拽
#[derive(Resource, Default)]
struct MidDrag {
    down: bool,
    last: Vec2,
}

pub struct InputPlugin;

impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Selection>()
            .init_resource::<UiHitZones>()
            .init_resource::<DragState>()
            .init_resource::<MidDrag>()
            .add_systems(Update, mouse_buttons.run_if(in_state(AppState::Map)))
            .add_systems(Update, keyboard_shortcuts);
    }
}

fn over_ui(cursor: Option<Vec2>, zones: &UiHitZones) -> bool {
    match cursor {
        Some(c) => zones.zones.iter().any(|(r, _)| r.contains(c)),
        None => false,
    }
}

/// 拾取：屏幕 18px 容差内最近的可见单位（红方须已被探测）
fn pick_unit(world: Vec2, mpp: f32, units: &mut Query<(Entity, &mut Unit, &Position)>) -> Option<Entity> {
    let tol = 18.0 * mpp;
    let mut best: Option<(f32, Entity)> = None;
    for (e, u, p) in units.iter() {
        if u.side == Side::Red && !u.detected_by_blue {
            continue;
        }
        let d = p.0.distance(world);
        if d <= tol && best.map_or(true, |(bd, _)| d < bd) {
            best = Some((d, e));
        }
    }
    best.map(|(_, e)| e)
}

fn mouse_buttons(
    buttons: Res<ButtonInput<MouseButton>>,
    mut drag: ResMut<DragState>,
    mut mid: ResMut<MidDrag>,
    cursor: Res<CursorState>,
    zones: Res<UiHitZones>,
    mut rig: ResMut<CameraRig>,
    mut selection: ResMut<Selection>,
    mut units: Query<(Entity, &mut Unit, &Position)>,
    window: Query<&Window, With<PrimaryWindow>>,
) {
    let _ = window;
    let cs = cursor.screen;

    // ---- 左键：点击选择 / 拖拽平移 ----
    if buttons.pressed(MouseButton::Left) {
        if let Some(c) = cs {
            if !drag.down {
                drag.down = true;
                drag.last = c;
                drag.moved = 0.0;
            } else {
                let d = c - drag.last;
                drag.moved += d.length();
                if drag.moved > 6.0 && !over_ui(cs, &zones) {
                    rig.pan_by_screen_delta(d);
                }
                drag.last = c;
            }
        }
    } else if drag.down {
        // 释放：位移小视为点击
        if drag.moved <= 6.0 && !over_ui(cs, &zones) {
            if let Some(w) = cursor.world {
                match pick_unit(w, rig.mpp, &mut units) {
                    Some(e) => selection.0 = Some(e),
                    None => selection.0 = None,
                }
            }
        }
        drag.down = false;
    }

    // ---- 中键：拖拽平移 ----
    if buttons.pressed(MouseButton::Middle) {
        if let Some(c) = cs {
            if !mid.down {
                mid.down = true;
                mid.last = c;
            } else {
                let d = c - mid.last;
                rig.pan_by_screen_delta(d);
                mid.last = c;
            }
        }
    } else {
        mid.down = false;
    }

    // ---- 右键：对所选蓝方移动单位下达机动命令 ----
    if buttons.just_pressed(MouseButton::Right) && !over_ui(cs, &zones) {
        if let (Some(dest), Some(sel)) = (cursor.world, selection.0) {
            if let Ok((_, mut u, _)) = units.get_mut(sel) {
                if u.side == Side::Blue && u.kind.mobile() {
                    u.order_move(dest);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn keyboard_shortcuts(
    keys: Res<ButtonInput<bevy::prelude::KeyCode>>,
    mut clock: ResMut<SimClock>,
    mut selection: ResMut<Selection>,
    mut rig: ResMut<CameraRig>,
    mut globe: ResMut<GlobeRig>,
    mut next: ResMut<NextState<AppState>>,
    state: Res<State<AppState>>,
    ctx: Res<MapCtx>,
    mode: Res<RenderMode>,
    window: Query<&bevy::window::Window, With<PrimaryWindow>>,
    mut units: Query<(Entity, &mut Unit, &Position)>,
) {
    if keys.just_pressed(bevy::prelude::KeyCode::Space) {
        clock.paused = !clock.paused;
    }
    if keys.just_pressed(bevy::prelude::KeyCode::Equal)
        || keys.just_pressed(bevy::prelude::KeyCode::NumpadAdd)
    {
        clock.speed_idx = (clock.speed_idx + 1).min(TIME_SPEEDS.len() - 1);
    }
    if keys.just_pressed(bevy::prelude::KeyCode::Minus)
        || keys.just_pressed(bevy::prelude::KeyCode::NumpadSubtract)
    {
        clock.speed_idx = clock.speed_idx.saturating_sub(1);
    }
    if keys.just_pressed(bevy::prelude::KeyCode::Escape) {
        selection.0 = None;
    }
    // G：地图 ⇄ 地球（仅窗口模式；地球侧的 G 由 globe_controls 处理）
    if keys.just_pressed(bevy::prelude::KeyCode::KeyG)
        && *mode == RenderMode::Window
        && *state.get() == AppState::Map
    {
        let (lat, lon) = ctx.proj.unproject(rig.target);
        let vp_h = window.single().map(|w| w.height()).unwrap_or(900.0);
        globe.lat = lat as f32;
        globe.lon = lon as f32;
        globe.distance = distance_for_mpp(MAP_MAX_MPP * 1.06, vp_h);
        next.set(AppState::Globe);
    }
    if keys.just_pressed(bevy::prelude::KeyCode::KeyF) {
        if let Some(sel) = selection.0 {
            if let Ok((_, _, p)) = units.get(sel) {
                focus_on(&mut rig, p.0);
            }
        }
    }
    if keys.just_pressed(bevy::prelude::KeyCode::KeyR) {
        if let Some(sel) = selection.0 {
            if let Ok((_, mut u, _)) = units.get_mut(sel) {
                if u.side == Side::Blue {
                    u.resume_patrol();
                }
            }
        }
    }
}

/// 距离/方位显示（“18.3km 224”）
pub fn range_bearing_text(from: Vec2, to: Vec2) -> String {
    let d = from.distance(to);
    let b = bearing_deg(from, to);
    if d >= 1000.0 {
        format!("{:.1}km {:03.0}", d / 1000.0, b)
    } else {
        format!("{:.0}m {:03.0}", d, b)
    }
}
