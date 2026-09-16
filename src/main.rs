//! MiaoSuan Decision Optimization System（妙算决策优化系统）
//!
//! 仿 CMO（Command: Modern Operations）的战术态势/决策推演原型：
//! Bevy 渲染 OSM 矢量地图，蓝红双方单位在真实地理底图上执行巡逻/机动，
//! 雷达/声纳/目视探测构成战争迷雾，提供接触列表与单位决策面板。

mod camera;
mod geo;
mod globe;
mod globe_tiles;
mod input;
mod map_render;
mod mvt;
mod osm;
mod satellites;
mod sim;
mod tiles;
mod web_cache;
mod timeline;
mod units_render;
mod weburl;
mod ui;

use bevy::asset::AssetServer;
use bevy::ecs::observer::On;
use bevy::state::state::{NextState, OnEnter};
use bevy::image::Image;
use bevy::prelude::*;
use bevy::camera::{ImageRenderTarget, RenderTarget};
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::render::view::screenshot::{save_to_disk, Screenshot, ScreenshotCaptured};
use bevy::window::{PrimaryWindow, WindowResolution};

use camera::CameraPlugin;
use geo::Projection;
use input::InputPlugin;
use sim::SimClock;
use ui::UiPlugin;

/// 地图上下文（投影 + 数据范围），供 UI 与交互换算
#[derive(Resource)]
pub struct MapCtx {
    pub proj: Projection,
    pub bounds: Rect,
}

/// 渲染/运行模式：地球↔地图的相机切换只在窗口模式下生效
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// 窗口交互（默认）：地球起始，可落入地图
    Window,
    /// 离屏渲染地图视图
    ImageMap,
    /// 离屏渲染地球视图
    ImageGlobe,
    /// UI 自检
    Selftest,
}

/// 启动配置（CLI 解析结果）
#[derive(Resource)]
struct Launch {
    globe_alt: Option<f32>,
    globe_view: Option<(f32, f32, Option<f32>)>,
    map_path: Option<String>,
    screenshot: Option<String>,
    render_image: Option<String>,
    render_globe: bool,
    selftest_ui: bool,
    frames: Option<u32>,
    zoom: Option<f32>,
}

/// URL hash 指定的初始视图
#[derive(Resource, Default)]
struct UrlView(Option<weburl::InitialView>);

/// 截图完成标记（观察器写入，退出系统读取）
#[derive(Resource, Default)]
struct ShotDone(bool);

/// 离屏渲染任务
#[derive(Resource)]
struct RenderImageJob {
    handle: Handle<Image>,
    path: String,
    width: u32,
    height: u32,
    /// 需等待加载完成的资产（如地球贴图）
    wait: Option<Handle<Image>>,
}
#[derive(Resource, Default)]
struct RenderImageDone(bool);

fn parse_args() -> Launch {
    let mut globe_alt: Option<f32> = None;
    let mut globe_view: Option<(f32, f32, Option<f32>)> = None;
    let mut launch = Launch {
        globe_alt: None,
        globe_view: None,
        map_path: None,
        screenshot: None,
        render_image: None,
        render_globe: false,
        selftest_ui: false,
        frames: None,
        zoom: None,
    };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--map" => {
                if let Some(v) = it.next() {
                    launch.map_path = Some(v);
                }
            }
            "--screenshot" => {
                if let Some(v) = it.next() {
                    launch.screenshot = Some(v);
                }
            }
            "--selftest-ui" => {
                launch.selftest_ui = true;
                if launch.render_image.is_none() {
                    launch.render_image = Some("/tmp/selftest_ui.png".into());
                }
            }
            "--render-globe" => {
                launch.render_globe = true;
                // 可选路径参数：不以 -- 开头则视为输出路径
                if let Some(p) = it.peek() {
                    if !p.starts_with("--") {
                        launch.render_image = Some(it.next().unwrap());
                    }
                }
                if launch.render_image.is_none() {
                    launch.render_image = Some("/tmp/globe.png".into());
                }
            }
            "--render-image" => {
                if let Some(v) = it.next() {
                    launch.render_image = Some(v);
                }
            }
            "--frames" => {
                if let Some(v) = it.next() {
                    launch.frames = v.parse().ok();
                }
            }
            "--globe-alt" => {
                if let Some(v) = it.next() {
                    globe_alt = v.parse().ok();
                }
            }
            "--globe-view" => {
                // lat,lon[,alt_m]：诊断用初始地球视角
                if let Some(v) = it.next() {
                    let p: Vec<f32> = v.split(',').filter_map(|x| x.parse().ok()).collect();
                    if p.len() >= 2 {
                        globe_view = Some((p[0], p[1], p.get(2).copied()));
                    }
                }
            }
            "--zoom" => {
                if let Some(v) = it.next() {
                    launch.zoom = v.parse().ok();
                }
            }
            _ => {}
        }
    }
    launch.globe_alt = globe_alt;
    launch.globe_view = globe_view;
    launch
}

#[bevy_main]
fn main() {
    use bevy::asset::{embedded_asset, load_embedded_asset};

    let launch = parse_args();
    let globe_alt = launch.globe_alt;
    let diag_view = launch.globe_view;
    let mode = if launch.selftest_ui {
        RenderMode::Selftest
    } else if launch.render_globe {
        RenderMode::ImageGlobe
    } else if launch.render_image.is_some() {
        RenderMode::ImageMap
    } else {
        RenderMode::Window
    };
    let mut initial_state = if mode == RenderMode::ImageMap { globe::AppState::Map } else { globe::AppState::Globe };
    // web 端：从 URL hash 恢复初始视图（#map=lat,lon,mpp / #globe=lat,lon）
    let mut url_view = None;
    if mode == RenderMode::Window {
        if let Some(view) = weburl::read_initial_view() {
            match view {
                weburl::InitialView::Map { .. } => initial_state = globe::AppState::Map,
                weburl::InitialView::Globe { .. } => initial_state = globe::AppState::Globe,
            }
            url_view = Some(view);
        }
    }

    let mut app = App::new();
    app
        .add_plugins(
            DefaultPlugins
                .set(asset_plugin_config())
                .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "MiaoSuan Decision Optimization System".into(),
                    resolution: WindowResolution::new(1600, 900),
                    // web 端渲染到指定 canvas；桌面端忽略
                    canvas: Some("#msdos-canvas".into()),
                    fit_canvas_to_parent: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        )
        .insert_resource(launch)
        .insert_resource(mode)
        .init_state::<globe::AppState>()
        .insert_resource(NextState::Pending(initial_state))
        .init_resource::<globe::GlobeRig>()
        .init_resource::<SimClock>()
        .init_resource::<tiles::TileCache>()
        .init_resource::<tiles::TileSource>()
        .init_resource::<globe::DataRing>()
        .insert_resource(tiles::LiveMap { enabled: true })
        .init_resource::<ShotDone>()
        .init_resource::<RenderImageDone>()
        .add_plugins(CameraPlugin)
        .add_plugins(InputPlugin)
        .add_plugins(UiPlugin)
        .add_systems(bevy::app::PostStartup, timeline::build_timeline)
        .add_systems(OnEnter(globe::AppState::Globe), globe::enter_globe_cameras)
        .add_systems(OnEnter(globe::AppState::Map), globe::enter_map_cameras)
        .add_systems(
            Startup,
            (setup_world, units_render::init_unit_visuals).chain(),
        )
        .add_systems(
            Update,
            (
                advance_clock,
                sim::movement_system,
                sim::detection_system,
                units_render::spawn_unit_visuals,
                units_render::sync_symbols,
                units_render::sync_leaders,
                units_render::sync_labels,
                units_render::sync_selection,
                units_render::sync_routes,
                auto_screenshot,
                render_image_trigger,
                auto_exit,
            )
                .chain(),
        )
        .add_systems(Update, globe::spawn_globe_markers)
        .add_systems(
            Update,
            (
                globe::globe_controls,
                globe::sync_globe_camera,
                globe::sync_globe_markers,
            )
                .chain()
                .run_if(in_state(globe::AppState::Globe)),
        )
        .init_resource::<globe_tiles::GlobeTileCache>()
        .init_resource::<satellites::SatLayer>()
        .init_resource::<timeline::TimeWindow>()
        .add_systems(
            Update,
            globe_tiles::globe_tile_system.run_if(in_state(globe::AppState::Globe)),
        )
        .add_systems(
            Update,
            satellites::sat_stream_system,
        )
        .add_systems(Update, globe::map_takeoff.run_if(in_state(globe::AppState::Map)))
        .add_systems(
            Update,
            (timeline::timeline_system, timeline::timeline_hit_zone).chain(),
        )
        .add_systems(
            Update,
            tiles::tile_stream_system.run_if(in_state(globe::AppState::Map)),
        )
        .add_systems(Update, globe::sync_data_ring.run_if(in_state(globe::AppState::Globe)))
        .add_systems(Update, (apply_url_view, weburl::sync_url_system).chain());

    // URL 恢复视图交给 setup_world 应用（避免被默认视野覆盖）
    app.insert_resource(UrlView(url_view));

    // 诊断/验证：--globe-alt 指定地球初始视距
    if let Some(alt) = globe_alt {
        let mut rig = app.world_mut().resource_mut::<globe::GlobeRig>();
        rig.distance = globe::GLOBE_RADIUS + alt;
    }
    // 诊断/验证：--globe-view lat,lon[,alt] 指定初始地球视角
    if let Some((lat, lon, alt)) = diag_view {
        let mut rig = app.world_mut().resource_mut::<globe::GlobeRig>();
        rig.lat = lat.clamp(-89.0, 89.0);
        rig.lon = lon;
        if let Some(a) = alt {
            rig.distance = globe::GLOBE_RADIUS + a;
        }
    }

    // 地球贴图内嵌进二进制（wasm 免网络加载、免 .meta 探测；桌面端同样可用）
    bevy::asset::embedded_asset!(&mut app, "src/", "../assets/earth_2048.jpg");
    let server: bevy::asset::AssetServer = app.world().resource::<bevy::asset::AssetServer>().clone();
    let earth = bevy::asset::load_embedded_asset!(&server, "../assets/earth_2048.jpg");
    app.insert_resource(globe::EarthTexture(earth));

    app.run();
}

/// 资产根目录：桌面端锚定到项目 assets/（0.19 默认相对可执行文件）；web 端保持相对路径
fn asset_plugin_config() -> bevy::asset::AssetPlugin {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let root = std::env::current_dir()
            .expect("无法获取工作目录")
            .join("assets");
        bevy::asset::AssetPlugin {
            file_path: root.to_string_lossy().into_owned(),
            ..Default::default()
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        bevy::asset::AssetPlugin::default()
    }
}

/// 首帧应用 URL 恢复视图（Startup 时 fit_canvas_to_parent 未生效、窗口高度仍是默认值，
/// 视高→mpp 换算需等窗口尺寸稳定）
fn apply_url_view(
    mut done: Local<bool>,
    mut frames: Local<u32>,
    mut url_view: Option<ResMut<UrlView>>,
    mut rig: ResMut<camera::CameraRig>,
    mut globe_rig: ResMut<globe::GlobeRig>,
    window: Query<&bevy::window::Window, With<PrimaryWindow>>,
) {
    if *done {
        return;
    }
    *frames += 1;
    // fit_canvas_to_parent 的 ResizeObserver 在首帧后才回调；
    // 等窗口尺寸偏离默认（1600×900）或超时 30 帧再换算视高
    let size_ready = window.single().map_or(true, |w| {
        *frames > 30 || (w.width() - 1600.0).abs() > 1.0 || (w.height() - 900.0).abs() > 1.0
    });
    if !size_ready {
        return;
    }
    let Some(view) = url_view.as_deref_mut() else { *done = true; return };
    let Some(v) = view.0.take() else { *done = true; return };
    let win_h = window.single().map(|w| w.height()).unwrap_or(900.0);
    match v {
        weburl::InitialView::Map { lat, lon, alt_m } => {
            let global = Projection::global();
            rig.target = global.project(lat, lon);
            rig.mpp = (alt_m / win_h).clamp(1.2, globe::MAP_MAX_MPP);
        }
        weburl::InitialView::Globe { lat, lon, alt_m } => {
            globe_rig.lat = lat.clamp(-85.0, 85.0);
            globe_rig.lon = lon;
            globe_rig.distance = (globe::GLOBE_RADIUS + alt_m)
                .clamp(globe::GLOBE_RADIUS * 1.02, globe::GLOBE_RADIUS * 4.0);
        }
    }
    *done = true;
}

/// 场景时间推进
fn advance_clock(mut clock: ResMut<SimClock>, time: Res<Time>) {
    clock.t += clock.sim_dt(time.delta().as_secs_f32()) as f64;
}

/// 解析地图、生成图层与想定
fn setup_world(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut std_materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    earth: Res<globe::EarthTexture>,
    mode: Res<RenderMode>,
    launch: Res<Launch>,
    _url_view: Res<UrlView>,
    mut rig: ResMut<camera::CameraRig>,
    mut ring: ResMut<globe::DataRing>,
    window: Query<&bevy::window::Window, With<PrimaryWindow>>,
) {
    if launch.selftest_ui {
        // 最小 UI 自检：全屏红色节点 + 白色文字
        let size = Extent3d { width: 400, height: 300, ..default() };
        let mut image = Image::new_fill(
            size,
            TextureDimension::D2,
            &[0, 0, 0, 255],
            TextureFormat::Bgra8UnormSrgb,
            bevy::asset::RenderAssetUsages::all(),
        );
        image.texture_descriptor.usage |=
            TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC;
        let handle = images.add(image);
        commands.spawn((
            Camera2d,
            RenderTarget::Image(ImageRenderTarget { handle: handle.clone(), scale_factor: 1.0 }),
            Msaa::Off,
        ));
        commands.spawn((
            bevy::ui::Node {
                width: bevy::ui::Val::Percent(100.0),
                height: bevy::ui::Val::Percent(100.0),
                ..default()
            },
            bevy::ui::BackgroundColor(Color::srgb(0.8, 0.1, 0.1)),
        ));
        commands.spawn((
            Text::new("UI TEST 12345"),
            TextFont::from_font_size(40.0),
            TextColor(Color::WHITE),
        ));
        commands.insert_resource(RenderImageJob {
            handle,
            path: "/tmp/selftest_ui.png".into(),
            width: 400,
            height: 300,
            wait: None,
        });
        commands.insert_resource(MapCtx {
            proj: Projection::new(0.0, 0.0),
            bounds: Rect::new(-1.0, -1.0, 1.0, 1.0),
        });
        return;
    }
    // 离屏渲染共用：创建 1600x900 图像目标
    let offscreen = launch.render_image.as_ref().map(|_| {
        let size = Extent3d { width: 1600, height: 900, ..default() };
        let mut image = Image::new_fill(
            size,
            TextureDimension::D2,
            &[0, 0, 0, 255],
            TextureFormat::Bgra8UnormSrgb,
            bevy::asset::RenderAssetUsages::all(),
        );
        image.texture_descriptor.usage |=
            TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC;
        images.add(image)
    });

    // 全球统一 Web Mercator 坐标；大地背景全球铺底
    let proj = Projection::global();
    tiles::spawn_global_background(&mut commands, &mut meshes, &mut materials);

    let bounds = if let Some(path) = &launch.map_path {
        // 静态模式：预载整个文件（离线可用），关闭在线瓦片流
        commands.insert_resource(tiles::LiveMap { enabled: false });
        let xml = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("无法读取地图文件 {path}: {e}"));
        let data = osm::parse_osm(&xml).unwrap_or_else(|e| panic!("地图解析失败（{path}）: {e}"));
        let data_proj = data.projection().expect("地图不含任何节点");
        let (clat, clon) = (
            (data_proj.lat0),
            (data_proj.lon0),
        );
        let local = Projection::new(clat, clon);
        let map = osm::extract_map(&data, &local);
        eprintln!(
            "OSM(静态): {} 节点 / {} 面 / {} 线",
            data.nodes.len(),
            map.polys.len(),
            map.lines.len()
        );
        let origin = proj.project(clat, clon);
        let shared = map_render::white_vertex_material(&mut materials);
        map_render::spawn_map_layers_at(
            &mut commands,
            &mut meshes,
            &mut materials,
            map_render::build_map_mesh(&map, &local, map_render::graticule_width_for_bounds(map.max.y - map.min.y)),
            origin,
            &shared,
        );
        let w_min = data_proj.unproject(map.min);
        let w_max = data_proj.unproject(map.max);
        let (s_, w_) = (w_min.0.min(w_max.0), w_min.1.min(w_max.1));
        let (n_, e_) = (w_min.0.max(w_max.0), w_min.1.max(w_max.1));
        ring.bbox = Some((s_, w_, n_, e_));
        Rect {
            min: proj.project(s_, w_),
            max: proj.project(n_, e_),
        }
    } else {
        // 实时模式：地图数据由瓦片流按需在线获取（Overpass）
        eprintln!("OSM(实时): 视图瓦片将按需从 Overpass API 加载");
        // 初始视野锚定珍珠港
        let c = proj.project(21.355, -157.925);
        Rect {
            min: c - Vec2::new(4000.0, 4000.0),
            max: c + Vec2::new(4000.0, 4000.0),
        }
    };
    sim::spawn_scenario(&mut commands, &proj);
    let ctx = MapCtx { proj, bounds };
    commands.insert_resource(MapCtx { proj: ctx.proj, bounds: ctx.bounds });

    match *mode {
        RenderMode::Window => {
            // 地球起始：2D 相机先失活，地球相机激活（初始状态 Globe）
            // WebGL2 下 MSAA4x 破坏 UI pass（经典兼容问题），相机级关闭
            commands.spawn((
                Camera2d,
                Camera { is_active: false, ..default() },
                Msaa::Off,
            ));
            globe::setup_globe(&mut commands, &mut meshes, &mut std_materials, &earth, None);
        }
        RenderMode::ImageMap => {
            let handle = offscreen.expect("ImageMap 模式需要离屏目标");
            commands.spawn((
                Camera2d,
                Camera { is_active: true, ..default() },
                RenderTarget::Image(ImageRenderTarget { handle: handle.clone(), scale_factor: 1.0 }),
            ));
            commands.insert_resource(RenderImageJob {
                handle,
                path: launch.render_image.clone().unwrap(),
                width: 1600,
                height: 900,
                wait: None,
            });
        }
        RenderMode::ImageGlobe => {
            let handle = offscreen.expect("ImageGlobe 模式需要离屏目标");
            globe::setup_globe(
                &mut commands,
                &mut meshes,
                &mut std_materials,
                &earth,
                Some(handle.clone()),
            );
            commands.insert_resource(RenderImageJob {
                handle,
                path: launch.render_image.clone().unwrap(),
                width: 1600,
                height: 900,
                wait: None, // 贴图已内嵌，无需等待
            });
        }
        RenderMode::Selftest => unreachable!("selftest 已提前返回"),
    }

    // 初始视野：整幅地图（URL hash 指定时优先）
    let win_h = window.single().map(|w| w.height()).unwrap_or(900.0);
    rig.target = bounds.center();
    rig.mpp = launch.zoom.unwrap_or_else(|| {
        ((bounds.height() * 1.18) / win_h).clamp(1.2, globe::MAP_MAX_MPP)
    });

}

/// 自动截图（启动后等几帧让字体/布局就绪）
fn auto_screenshot(
    mut commands: Commands,
    launch: Res<Launch>,
    mut frames: Local<u32>,
    done: Res<ShotDone>,
) {
    let Some(path) = &launch.screenshot else { return };
    if done.0 {
        return;
    }
    *frames += 1;
    if *frames == 120 {
        let path = path.clone();
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path))
            .observe(|_: On<ScreenshotCaptured>, mut done: ResMut<ShotDone>| {
                done.0 = true;
            });
    }
}

/// 离屏渲染：场景就绪后挂 GPU 回读，拿到像素即存盘
fn render_image_trigger(
    mut commands: Commands,
    server: Res<AssetServer>,
    job: Option<Res<RenderImageJob>>,
    cache: Option<Res<tiles::TileCache>>,
    globe_cache: Option<Res<globe_tiles::GlobeTileCache>>,
    live: Res<tiles::LiveMap>,
    mut frames: Local<u32>,
    mut stable: Local<u32>,
) {
    let Some(job) = job else { return };
    *frames += 1;
    let asset_ready = job
        .wait
        .as_ref()
        .map(|h| server.is_loaded_with_dependencies(h))
        .unwrap_or(true);
    // 地球贴图瓦片模式：全球瓦片加载安定后再截
    let globe_ready = globe_cache
        .as_deref()
        .map(|c| c.inflight == 0 && (c.loaded() >= c.capacity() || *frames > 900))
        .unwrap_or(true);
    // 实时瓦片模式：加载完成且无在途请求需稳定 90 帧（约 1.5s），
    // 避免在请求节流的间隙（inflight 短暂归零）截图到半成品
    let tiles_ready = match (&cache, live.enabled) {
        (Some(c), true) => {
            let ok = c.inflight == 0 && (c.loaded_count() > 0 || *frames > 18_000);
            *stable = if ok { stable.saturating_add(1) } else { 0 };
            *stable >= 90
        }
        _ => true,
    };
    if *frames > 30 && asset_ready && tiles_ready && globe_ready {
        commands
            .spawn(Readback::texture(job.handle.clone()))
            .observe(save_render_image);
    }
}

fn save_render_image(
    ev: On<ReadbackComplete>,
    mut commands: Commands,
    job: Res<RenderImageJob>,
    mut done: ResMut<RenderImageDone>,
) {
    if done.0 {
        return;
    }
    commands.entity(ev.entity).remove::<Readback>(); // 只读一次
    let data = &ev.data;
    let (w, h) = (job.width as usize, job.height as usize);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for px in data.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]); // BGRA -> RGB
    }
    match image::RgbImage::from_raw(w as u32, h as u32, rgb) {
        Some(img) => match img.save(&job.path) {
            Ok(_) => eprintln!("离屏渲染已保存到 {}", job.path),
            Err(e) => eprintln!("离屏渲染保存失败: {e}"),
        },
        None => eprintln!("离屏渲染像素数据尺寸异常"),
    }
    done.0 = true;
}

/// 截图完成或到达帧数上限后退出（用于自动化验证）
fn auto_exit(
    mut frames: Local<u32>,
    launch: Res<Launch>,
    done: Res<ShotDone>,
    render_done: Res<RenderImageDone>,
    mut exit: MessageWriter<AppExit>,
) {
    *frames += 1;
    let want_shot = launch.screenshot.is_some();
    let shot_pending = want_shot && !done.0;
    let want_render = launch.render_image.is_some();
    if want_render && render_done.0 {
        exit.write(AppExit::Success);
        return;
    }
    if let Some(limit) = launch.frames {
        if *frames >= limit && !shot_pending {
            exit.write(AppExit::Success);
        }
    } else if want_shot && done.0 {
        exit.write(AppExit::Success);
    }
}
