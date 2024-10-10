mod adwaita_app;
mod hal_custom;
mod render;

use std::{
    any::type_name,
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Arc,
    },
    thread,
};

use adwaita_app::{WindowCommand, WindowOpen};
use atomicbox::AtomicOptionBox;
use bevy::{
    ecs::system::EntityCommand,
    prelude::*,
    render::{
        camera::{ManualTextureViewHandle, ManualTextureViews, RenderTarget},
        renderer::RenderDevice,
        settings::WgpuSettings,
        Extract, Render, RenderApp, RenderPlugin, RenderSet,
    },
    window::WindowRef,
};
use render::{DmabufInfo, FrameInfo};

#[derive(Debug, Clone)]
pub struct AdwaitaWindowPlugin {
    pub primary_window_config: Option<AdwaitaWindowConfig>,
}

impl Default for AdwaitaWindowPlugin {
    fn default() -> Self {
        Self {
            primary_window_config: Some(AdwaitaWindowConfig::default()),
        }
    }
}

impl Plugin for AdwaitaWindowPlugin {
    fn build(&self, app: &mut App) {
        let (send_window_open, recv_window_open) = flume::bounded::<WindowOpen>(1);
        thread::spawn(|| adwaita_app::main_thread_loop(recv_window_open));

        app.insert_resource(SendWindowOpen(send_window_open))
            .add_systems(PreUpdate, poll_windows)
            .observe(update_default_camera_render_target)
            .observe(update_existing_cameras_render_target);

        let render_app = app.sub_app_mut(RenderApp);
        render_app
            .add_systems(ExtractSchedule, extract_windows)
            .add_systems(Render, send_frame_info_to_windows.after(RenderSet::Render));

        if let Some(config) = self.primary_window_config.clone() {
            let world = app.world_mut();
            let entity = world.spawn_empty().id();
            AdwaitaWindow::open(config).apply(entity, world);
            world.entity_mut(entity).insert(PrimaryAdwaitaWindow);
        }
    }
}

impl AdwaitaWindowPlugin {
    #[must_use]
    pub fn render_plugin(settings: WgpuSettings) -> RenderPlugin {
        let render_creation = render::create_renderer(settings);
        RenderPlugin {
            render_creation,
            synchronous_pipeline_compilation: false,
        }
    }
}

#[derive(Debug, Component)]
pub struct AdwaitaWindow {
    send_command: flume::Sender<WindowCommand>,
    render_target_width: Arc<AtomicI32>,
    render_target_height: Arc<AtomicI32>,
    shared_next_frame: Arc<AtomicOptionBox<FrameInfo>>,
    closed: Arc<AtomicBool>,
    render_target_handle: ManualTextureViewHandle,
    last_render_target_size: UVec2,
    // use an `AtomicOptionBox` instead of `Option` because we only have a shared ref
    // during extract, and we want to `take` there
    next_frame_info: AtomicOptionBox<FrameInfo>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Component, Reflect)]
#[reflect(Default, Component)]
pub struct PrimaryAdwaitaWindow;

#[derive(Debug, Clone, Reflect)]
#[reflect(Default)]
pub struct AdwaitaWindowConfig {
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub resizable: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub header_bar: AdwaitaHeaderBar,
}

impl Default for AdwaitaWindowConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            title: "App".into(),
            resizable: true,
            maximized: false,
            fullscreen: false,
            header_bar: AdwaitaHeaderBar::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Default)]
pub enum AdwaitaHeaderBar {
    #[default]
    Full,
    OverContent,
    None,
}

#[derive(Debug, Resource)]
struct SendWindowOpen(flume::Sender<WindowOpen>);

impl AdwaitaWindow {
    #[must_use]
    pub fn open(config: AdwaitaWindowConfig) -> impl EntityCommand {
        move |entity, world: &mut World| {
            info!(
                "Creating new Adwaita window \"{}\" ({entity})",
                config.title
            );

            let (send_command, recv_command) = flume::bounded::<WindowCommand>(16);
            let render_target_width = Arc::new(AtomicI32::new(-1));
            let render_target_height = Arc::new(AtomicI32::new(-1));
            let shared_next_frame = Arc::new(AtomicOptionBox::<FrameInfo>::none());
            let closed = Arc::new(AtomicBool::new(false));
            let request = WindowOpen {
                config,
                recv_command,
                render_target_width: render_target_width.clone(),
                render_target_height: render_target_height.clone(),
                shared_next_frame: shared_next_frame.clone(),
                closed: closed.clone(),
            };

            let manual_texture_views = world.resource::<ManualTextureViews>();
            let render_target_handle = loop {
                let handle = ManualTextureViewHandle(rand::random());
                if !manual_texture_views.contains_key(&handle) {
                    break handle;
                }
            };

            world.entity_mut(entity).insert(AdwaitaWindow {
                send_command,
                render_target_width,
                render_target_height,
                shared_next_frame,
                closed,
                render_target_handle,
                last_render_target_size: UVec2::new(0, 0),
                next_frame_info: AtomicOptionBox::none(),
            });
            world
                .resource::<SendWindowOpen>()
                .0
                .send(request)
                .expect("Adwaita main thread dropped");
        }
    }

    #[must_use]
    pub const fn render_target_handle(&self) -> ManualTextureViewHandle {
        self.render_target_handle
    }

    #[must_use]
    pub const fn render_target(&self) -> RenderTarget {
        RenderTarget::TextureView(self.render_target_handle)
    }
}

fn update_default_camera_render_target(
    trigger: Trigger<OnInsert, Camera>,
    mut cameras: Query<&mut Camera>,
    primary_windows: Query<&AdwaitaWindow, With<PrimaryAdwaitaWindow>>,
) {
    let Ok(primary_window) = primary_windows.get_single() else {
        return;
    };

    let entity = trigger.entity();
    let mut camera = cameras
        .get_mut(entity)
        .expect("we are inserting this component into this entity");

    if matches!(camera.target, RenderTarget::Window(WindowRef::Primary)) {
        camera.target = primary_window.render_target();
    }
}

fn update_existing_cameras_render_target(
    trigger: Trigger<OnInsert, PrimaryAdwaitaWindow>,
    windows: Query<&AdwaitaWindow>,
    mut cameras: Query<&mut Camera>,
) {
    let entity = trigger.entity();
    let window = windows.get(entity).unwrap_or_else(|_| {
        panic!(
            "inserting `{}` onto {entity} without `{}`",
            type_name::<PrimaryAdwaitaWindow>(),
            type_name::<AdwaitaWindow>()
        )
    });

    for mut camera in &mut cameras {
        if matches!(camera.target, RenderTarget::Window(WindowRef::Primary)) {
            camera.target = window.render_target();
        }
    }
}

fn poll_windows(
    mut commands: Commands,
    mut windows: Query<(Entity, &mut AdwaitaWindow)>,
    render_device: Res<RenderDevice>,
    mut manual_texture_views: ResMut<ManualTextureViews>,
) {
    for (entity, mut window) in &mut windows {
        if window.closed.load(Ordering::SeqCst) {
            info!("Closing window {entity} due to Adwaita window being closed");
            commands.entity(entity).despawn_recursive();
            continue;
        }

        let (width, height) = (
            window.render_target_width.load(Ordering::SeqCst),
            window.render_target_height.load(Ordering::SeqCst),
        );
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            continue;
        };

        let size = UVec2::new(width.max(1), height.max(1));
        if size == window.last_render_target_size {
            continue;
        }
        window.last_render_target_size = size;

        let (manual_texture_view, dmabuf_fd) =
            render::setup_render_target(size, render_device.as_ref());
        // give a shared ref of this texture view to the Adwaita app
        // so that, even if *we* drop it while the window is rendering this frame,
        // the GPU resources won't be deallocated until the window *also* drops it
        let texture_view = manual_texture_view.texture_view.clone();
        manual_texture_views.insert(window.render_target_handle.clone(), manual_texture_view);
        window.next_frame_info.store(
            Some(Box::new(FrameInfo {
                dmabuf: DmabufInfo {
                    size,
                    fd: dmabuf_fd,
                },
                texture_view,
            })),
            Ordering::SeqCst,
        );
    }
}

#[derive(Debug, Component)]
struct RenderWindow {
    shared_next_frame: Arc<AtomicOptionBox<FrameInfo>>,
    next_frame_info: Option<FrameInfo>,
}

fn extract_windows(mut commands: Commands, windows: Extract<Query<&AdwaitaWindow>>) {
    for window in &windows {
        let Some(next_frame_info) = window.next_frame_info.take(Ordering::SeqCst) else {
            continue;
        };

        commands.spawn(RenderWindow {
            shared_next_frame: window.shared_next_frame.clone(),
            next_frame_info: Some(*next_frame_info),
        });
    }
}

fn send_frame_info_to_windows(mut windows: Query<&mut RenderWindow>) {
    for mut window in &mut windows {
        let Some(next_frame_info) = window.next_frame_info.take() else {
            continue;
        };

        window
            .shared_next_frame
            .store(Some(Box::new(next_frame_info)), Ordering::SeqCst);
    }
}
