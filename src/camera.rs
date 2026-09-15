//! 相机装备（rig）：平移、滚轮缩放（锚定光标）、键盘漫游。
//!
//! 世界的“米/像素”由 `CameraRig::mpp` 单一变量表达，
//! 每帧写入 OrthographicProjection.scale 与相机 Transform，保持完全一致，
//! 光标世界坐标因此可以直接由 rig 精确算出（无帧延迟）。

use bevy::input::mouse::AccumulatedMouseScroll;
use bevy::prelude::*;
use bevy::state::condition::in_state;
use bevy::window::PrimaryWindow;

use crate::globe::AppState;

#[derive(Resource)]
pub struct CameraRig {
    /// 视野中心（世界坐标，米）
    pub target: Vec2,
    /// 每像素对应的米数（越大越缩小）
    pub mpp: f32,
}

impl Default for CameraRig {
    fn default() -> Self {
        CameraRig { target: Vec2::ZERO, mpp: 20.0 }
    }
}

impl CameraRig {
    pub fn world_from_screen(&self, cursor: Vec2, viewport: Vec2) -> Vec2 {
        let d = cursor - viewport * 0.5;
        self.target + Vec2::new(d.x, -d.y) * self.mpp
    }

    pub fn pan_by_screen_delta(&mut self, delta_px: Vec2) {
        self.target += Vec2::new(-delta_px.x, delta_px.y) * self.mpp;
    }

    /// 以光标为锚缩放，保持光标下的世界点不动
    pub fn zoom_at(&mut self, cursor: Vec2, viewport: Vec2, factor: f32) {
        let anchor = self.world_from_screen(cursor, viewport);
        self.mpp = (self.mpp / factor).clamp(1.2, 1200.0);
        let now = self.world_from_screen(cursor, viewport);
        self.target += anchor - now;
    }
}

/// 每帧更新的光标状态
#[derive(Resource, Default)]
pub struct CursorState {
    pub screen: Option<Vec2>,
    pub world: Option<Vec2>,
}

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CameraRig>()
            .init_resource::<CursorState>()
            .add_systems(Update, update_cursor)
            .add_systems(
                Update,
                (keyboard_pan, wheel_zoom, apply_rig)
                    .chain()
                    .run_if(in_state(AppState::Map)),
            );
    }
}

fn update_cursor(
    mut cursor: ResMut<CursorState>,
    rig: Res<CameraRig>,
    window: Query<&Window, With<PrimaryWindow>>,
) {
    let Ok(w) = window.single() else { return };
    let viewport = Vec2::new(w.width(), w.height());
    cursor.screen = w.cursor_position();
    cursor.world = cursor.screen.map(|p| rig.world_from_screen(p, viewport));
}

fn keyboard_pan(
    keys: Res<ButtonInput<KeyCode>>,
    mut rig: ResMut<CameraRig>,
    time: Res<bevy::time::Time>,
) {
    let mut d = Vec2::ZERO;
    if keys.pressed(KeyCode::KeyW) || keys.pressed(KeyCode::ArrowUp) {
        d.y += 1.0;
    }
    if keys.pressed(KeyCode::KeyS) || keys.pressed(KeyCode::ArrowDown) {
        d.y -= 1.0;
    }
    if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
        d.x -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
        d.x += 1.0;
    }
    if d != Vec2::ZERO {
        let speed = 900.0 * rig.mpp * time.delta().as_secs_f32();
        rig.target += d.normalize() * speed;
    }
}

fn wheel_zoom(
    mut rig: ResMut<CameraRig>,
    scroll: Res<AccumulatedMouseScroll>,
    cursor: Res<CursorState>,
    zones: Res<crate::input::UiHitZones>,
    window: Query<&Window, With<PrimaryWindow>>,
) {
    if scroll.delta.y.abs() < 1e-4 {
        return;
    }
    // UI 区域（时间轴等）内的滚轮不缩放地图
    if cursor.screen.map(|c| zones.zones.iter().any(|(r, _)| r.contains(c))).unwrap_or(false) {
        return;
    }
    let Ok(w) = window.single() else { return };
    let viewport = Vec2::new(w.width(), w.height());
    let pos = cursor.screen.unwrap_or(viewport * 0.5);
    let factor = 1.0 + scroll.delta.y.clamp(-1.0, 1.0) * 0.15;
    rig.zoom_at(pos, viewport, factor.max(0.2));
}

/// F 键聚焦逻辑由 input.rs 调用 `focus_on` 完成
fn apply_rig(rig: Res<CameraRig>, mut q: Query<(&mut Transform, &mut Projection), With<Camera2d>>) {
    for (mut t, mut proj) in &mut q {
        t.translation.x = rig.target.x;
        t.translation.y = rig.target.y;
        if let Projection::Orthographic(o) = &mut *proj {
            o.scale = rig.mpp;
        }
    }
}

/// 供 input.rs 调用的聚焦逻辑
pub fn focus_on(rig: &mut CameraRig, pos: Vec2) {
    rig.target = pos;
}
