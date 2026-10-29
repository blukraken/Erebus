//! The window. Renders on demand only: resize, DPI change, `P`, a link change, a close
//! button state change, while the bead fade runs, or in fullscreen while the cursor moves.
//! With nothing changing, the loop sleeps in `ControlFlow::Wait`.
//!
//! Suspend signals on Windows (winit 0.30.13, wgpu 30 on D3D12):
//! - Minimise arrives as `Resized(0x0)` from `WM_SIZE`. Rendering stops until a non-zero size.
//! - `WindowEvent::Occluded` is never emitted on Windows; winit lists it as unsupported.
//! - wgpu's D3D12 backend never returns `CurrentSurfaceTexture::Occluded`.
//!
//! A covered window gets no signal, but it also draws nothing unless something changes.
//!
//! Close (the × button, Ctrl+Q, Alt+F4) hides the window to the core's tray while the
//! link is up, and exits while it is down: with no core there is no tray to come back from.
//!
//! The window is undecorated in both modes. winit's `focus_window` and its
//! fullscreen-at-creation path both synthesise an Alt keypress, so the face uses neither:
//! it creates the window plain, then goes fullscreen.
//!
//! Fullscreen presents unchanged frames while the cursor moves; see [`MOTION_FRAMES`].
//! `--no-motion-frames` turns that off.
//!
//! Every redraw request carries a reason. The first seconds after start or an F11 toggle
//! log each request and frame; `--trace` logs them all, plus per-second input counts.

use crate::gpu::Gpu;
use crate::link::{self, Link, LinkEvent};
use crate::render::{self, Close, Renderer, View};
use pinnos_protocol::FaceMsg;
use pinnos_theme as theme;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::windows::{Color, CornerPreference, WindowAttributesExtWindows, WindowExtWindows};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CursorIcon, Fullscreen, ResizeDirection, Window, WindowId};

const FADE_SECS: f32 = 0.3;
/// How long redraw reasons are logged after start or an F11 toggle.
const TRACE_SECS: u64 = 3;

/// How long fullscreen keeps presenting after the last cursor move.
///
/// Borderless fullscreen gets independent flip, and on a variable-refresh panel the refresh
/// rate then follows the face's presents. An idle face presents nothing, so the panel drops
/// to its minimum rate (48 Hz measured on the owner's G9) and the hardware cursor moves
/// only that often. Presenting while the cursor moves holds the panel at full refresh for
/// exactly as long as the cursor needs it; a still cursor costs nothing. A window that does
/// not cover the monitor stays composed instead, but then shell bars such as yasb no longer
/// treat the face as fullscreen and draw over it.
const MOTION_FRAMES: Duration = Duration::from_millis(100);
/// Redraw reasons that repeat every frame; traced as per-second counts, not lines.
const STEADY: [&str; 2] = ["idle present", "cursor motion"];

pub struct Options {
    /// Start in the F11 window instead of fullscreen.
    pub windowed: bool,
    /// Log every redraw, and per-second input and frame counts, for the whole session.
    pub trace: bool,
    /// Present an unchanged frame at this rate while idle; 0 is off.
    pub idle_present_hz: f64,
    /// Present nothing extra while the cursor moves in fullscreen.
    pub no_motion_frames: bool,
}

#[derive(Debug)]
pub enum UiEvent {
    Link(LinkEvent),
}

/// What the cursor is over.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hit {
    None,
    Close,
    /// The top rail: moves the window in windowed mode; a double-click toggles fullscreen.
    Rail,
    Edge(ResizeDirection),
}

struct Live {
    renderer: Renderer,
    config: wgpu::SurfaceConfiguration,
    surface: wgpu::Surface<'static>,
    gpu: Gpu,
    // Dropped last: the surface must go before its window.
    window: Arc<Window>,
}

/// Per-second counts for `--trace`.
#[derive(Default)]
struct Activity {
    since: Option<Instant>,
    moves: u32,
    frames: u32,
    idle_frames: u32,
    motion_frames: u32,
    cursor_sets: u32,
}

struct App {
    live: Option<Live>,
    link: Link,
    palette: bool,
    link_up: bool,
    /// Bead position, linear in time; drawn eased.
    link_t: f32,
    last_tick: Instant,
    mods: ModifiersState,
    minimized: bool,
    /// Hidden to the tray. Nothing renders until `Show`.
    hidden: bool,
    /// Set by F11; centres the window once its new size lands.
    centre_pending: bool,
    opts: Options,
    cursor: Option<PhysicalPosition<f64>>,
    cursor_icon: CursorIcon,
    close: Close,
    /// The left button went down on the close button and is still down.
    close_armed: bool,
    /// Last left press on the rail, for double-click detection.
    rail_press: Option<(Instant, PhysicalPosition<f64>)>,
    frames: u64,
    error: Option<String>,
    started: Instant,
    trace_until: Instant,
    /// Why the pending redraw was asked for. A frame with none is a system `WM_PAINT`.
    pending: Option<&'static str>,
    activity: Activity,
    idle_every: Option<Duration>,
    next_idle: Instant,
    /// Fullscreen presents until then; see [`MOTION_FRAMES`].
    motion_until: Option<Instant>,
    /// Cursor position since it last entered the window, for telling motion from a
    /// window appearing under a still cursor.
    last_move: Option<PhysicalPosition<f64>>,
}

pub fn run(opts: Options) -> Result<(), String> {
    let event_loop = EventLoop::<UiEvent>::with_user_event().build().map_err(|e| e.to_string())?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    let link = link::spawn(move |e| {
        let _ = proxy.send_event(UiEvent::Link(e));
    })?;

    let now = Instant::now();
    let idle_every = (opts.idle_present_hz > 0.0).then(|| Duration::from_secs_f64(1.0 / opts.idle_present_hz));
    if let Some(d) = idle_every {
        log::info!("idle present every {:.2} ms", d.as_secs_f64() * 1000.0);
    }
    let mut app = App {
        live: None,
        link,
        palette: false,
        link_up: false,
        link_t: 0.0,
        last_tick: now,
        mods: ModifiersState::empty(),
        minimized: false,
        hidden: false,
        centre_pending: false,
        opts,
        cursor: None,
        cursor_icon: CursorIcon::Default,
        close: Close::Rest,
        close_armed: false,
        rail_press: None,
        frames: 0,
        error: None,
        started: now,
        trace_until: now + Duration::from_secs(TRACE_SECS),
        pending: None,
        activity: Activity::default(),
        idle_every,
        next_idle: now,
        motion_until: None,
        last_move: None,
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    match app.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

fn resize_cursor(d: ResizeDirection) -> CursorIcon {
    match d {
        ResizeDirection::East | ResizeDirection::West => CursorIcon::EwResize,
        ResizeDirection::North | ResizeDirection::South => CursorIcon::NsResize,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::NeswResize,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::NwseResize,
    }
}

/// Per-mode window chrome: DWM shadow and rounded corners in the window, neither when
/// fullscreen. winit's shadow trims a pixel off the client top, which fullscreen must not lose.
fn set_chrome(w: &Window, fullscreen: bool) {
    w.set_undecorated_shadow(!fullscreen);
    w.set_corner_preference(if fullscreen { CornerPreference::DoNotRound } else { CornerPreference::Round });
}

fn hwnd(w: &Window) -> Option<windows_sys::Win32::Foundation::HWND> {
    match w.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as _),
        _ => None,
    }
}

/// Restores and raises the window for `CoreMsg::Show`. Windows grants the foreground only
/// if the core, which just took the tray click, passed its right on with
/// `AllowSetForegroundWindow(face pid)`. Otherwise the request is refused and the taskbar
/// button flashes. No input is synthesised.
fn raise(w: &Window) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    let Some(h) = hwnd(w) else { return false };
    // SAFETY: plain Win32 calls on this process's own live window.
    unsafe {
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE);
        }
        SetForegroundWindow(h);
        GetForegroundWindow() == h
    }
}

/// The system double-click time and slop rectangle.
fn double_click() -> (Duration, f64, f64) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXDOUBLECLK, SM_CYDOUBLECLK};
    // SAFETY: argument-free queries of system settings.
    unsafe {
        (
            Duration::from_millis(GetDoubleClickTime() as u64),
            GetSystemMetrics(SM_CXDOUBLECLK) as f64 / 2.0,
            GetSystemMetrics(SM_CYDOUBLECLK) as f64 / 2.0,
        )
    }
}

impl App {
    fn tracing(&self) -> bool {
        self.opts.trace || Instant::now() < self.trace_until
    }

    fn ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    fn redraw(&mut self, why: &'static str) {
        if self.hidden {
            return;
        }
        let Some(live) = &self.live else { return };
        if self.tracing() && !STEADY.contains(&why) {
            log::info!("+{} ms redraw: {why}", self.ms());
        }
        self.pending.get_or_insert(why);
        live.window.request_redraw();
    }

    /// Logs and resets the per-second counts once a second has passed. With `force`, logs
    /// whatever has gathered.
    fn flush_activity(&mut self, force: bool) {
        if !self.opts.trace {
            return;
        }
        let Some(since) = self.activity.since else { return };
        let dt = since.elapsed();
        if dt < Duration::from_secs(1) && !force {
            return;
        }
        let a = std::mem::take(&mut self.activity);
        log::info!(
            "+{} ms activity over {} ms: {} moves, {} frames, {} idle frames, {} motion frames, {} cursor sets",
            self.ms(),
            dt.as_millis(),
            a.moves,
            a.frames,
            a.idle_frames,
            a.motion_frames,
            a.cursor_sets
        );
    }

    fn count(&mut self, f: impl FnOnce(&mut Activity)) {
        if self.opts.trace {
            self.activity.since.get_or_insert_with(Instant::now);
            f(&mut self.activity);
        }
    }

    /// Advances the bead fade. Returns true while it still runs.
    fn tick(&mut self) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        let target = if self.link_up { 1.0 } else { 0.0 };
        let step = dt / FADE_SECS;
        self.link_t = if self.link_t < target {
            (self.link_t + step).min(target)
        } else {
            (self.link_t - step).max(target)
        };
        self.link_t != target
    }

    fn toggle_fullscreen(&mut self) {
        let Some(live) = &self.live else { return };
        let w = live.window.clone();
        self.trace_until = Instant::now() + Duration::from_secs(TRACE_SECS);
        if w.fullscreen().is_some() {
            w.set_fullscreen(None);
            set_chrome(&w, false);
            if let Some(m) = w.current_monitor() {
                let s = m.size();
                let _ = w.request_inner_size(PhysicalSize::new(s.width * 3 / 5, s.height * 3 / 5));
            }
            self.centre_pending = true;
            log::info!("windowed");
        } else {
            set_chrome(&w, true);
            w.set_fullscreen(Some(Fullscreen::Borderless(w.current_monitor())));
            log::info!("fullscreen");
        }
    }

    fn centre(&mut self) {
        self.centre_pending = false;
        let Some(live) = &self.live else { return };
        let w = &live.window;
        let Some(m) = w.current_monitor() else { return };
        let (mp, ms, os) = (m.position(), m.size(), w.outer_size());
        let x = mp.x + (ms.width as i32 - os.width as i32) / 2;
        let y = mp.y + (ms.height as i32 - os.height as i32) / 2;
        w.set_outer_position(PhysicalPosition::new(x, y));
    }

    /// Close: to the tray while the link is up, out otherwise.
    fn close(&mut self, event_loop: &ActiveEventLoop, why: &str) {
        if self.hidden {
            return;
        }
        if !self.link_up {
            log::info!("{why}: link down, so no tray; exiting");
            event_loop.exit();
            return;
        }
        let Some(live) = &self.live else { return };
        live.window.set_visible(false);
        self.hidden = true;
        self.close = Close::Rest;
        self.close_armed = false;
        self.rail_press = None;
        self.flush_activity(true);
        log::info!("{why}: hidden to tray; {} frames drawn", self.frames);
        self.link.send(FaceMsg::Hidden);
    }

    fn show(&mut self) {
        let Some(live) = &self.live else { return };
        let w = &live.window;
        w.set_visible(true);
        w.set_minimized(false);
        let foreground = raise(w);
        self.hidden = false;
        self.last_tick = Instant::now();
        log::info!("shown; foreground {}", if foreground { "granted" } else { "refused (taskbar flashes)" });
        self.redraw("shown");
        self.link.send(FaceMsg::Shown);
    }

    fn hit(&self, p: PhysicalPosition<f64>) -> Hit {
        let Some(live) = &self.live else { return Hit::None };
        let w = &live.window;
        let scale = w.scale_factor();
        let (cw, ch) = (live.config.width, live.config.height);
        let (x, y, wf, hf) = (p.x as f32, p.y as f32, cw as f32, ch as f32);

        if w.fullscreen().is_none() && !w.is_maximized() {
            let band = (7.0 * scale as f32).round();
            let corner = band * 2.5;
            let (l, r, t, b) = (x < band, x >= wf - band, y < band, y >= hf - band);
            let (lc, rc, tc, bc) = (x < corner, x >= wf - corner, y < corner, y >= hf - corner);
            let edge = if (t && lc) || (l && tc) {
                Some(ResizeDirection::NorthWest)
            } else if (t && rc) || (r && tc) {
                Some(ResizeDirection::NorthEast)
            } else if (b && lc) || (l && bc) {
                Some(ResizeDirection::SouthWest)
            } else if (b && rc) || (r && bc) {
                Some(ResizeDirection::SouthEast)
            } else if t {
                Some(ResizeDirection::North)
            } else if b {
                Some(ResizeDirection::South)
            } else if l {
                Some(ResizeDirection::West)
            } else if r {
                Some(ResizeDirection::East)
            } else {
                None
            };
            if let Some(d) = edge {
                return Hit::Edge(d);
            }
        }

        let c = render::chrome(cw, ch, scale);
        let [x0, y0, x1, y1] = c.close_hit;
        if x >= x0 && x < x1 && y >= y0 && y < y1 {
            Hit::Close
        } else if y < c.rail {
            Hit::Rail
        } else {
            Hit::None
        }
    }

    fn set_close(&mut self, state: Close) {
        if state != self.close {
            self.close = state;
            self.redraw("close button state");
        }
    }

    fn cursor_moved(&mut self, p: PhysicalPosition<f64>) {
        self.count(|a| a.moves += 1);
        // Windows also sends a move when a window appears under a still cursor; only a
        // change from a known position counts as motion.
        let moved = self.last_move.replace(p).is_some_and(|q| q != p);
        let fullscreen = self.live.as_ref().is_some_and(|l| l.window.fullscreen().is_some());
        if moved && fullscreen && !self.opts.no_motion_frames {
            self.motion_until = Some(Instant::now() + MOTION_FRAMES);
            if self.pending.is_none() {
                self.redraw("cursor motion");
            }
        }
        self.cursor = Some(p);
        let hit = self.hit(p);
        let over = hit == Hit::Close;
        self.set_close(match (self.close_armed, over) {
            (true, true) => Close::Press,
            (false, true) => Close::Hover,
            _ => Close::Rest,
        });
        let icon = if let Hit::Edge(d) = hit { resize_cursor(d) } else { CursorIcon::Default };
        if icon != self.cursor_icon {
            self.cursor_icon = icon;
            self.count(|a| a.cursor_sets += 1);
            if let Some(live) = &self.live {
                live.window.set_cursor(icon);
            }
        }
    }

    fn left_button(&mut self, event_loop: &ActiveEventLoop, state: ElementState) {
        // The last move position stands in for the press position; winit reports none.
        let Some(p) = self.cursor else { return };
        let hit = self.hit(p);
        if state == ElementState::Released {
            if self.close_armed {
                self.close_armed = false;
                if hit == Hit::Close {
                    self.close(event_loop, "close button");
                } else {
                    self.set_close(Close::Rest);
                }
            }
            return;
        }
        let Some(live) = &self.live else { return };
        let w = live.window.clone();
        match hit {
            Hit::Close => {
                self.close_armed = true;
                self.set_close(Close::Press);
            }
            Hit::Edge(d) => {
                log::info!("chrome: resize {d:?}");
                if let Err(e) = w.drag_resize_window(d) {
                    log::warn!("resize: {e}");
                }
            }
            Hit::Rail => {
                let now = Instant::now();
                let (dt, sx, sy) = double_click();
                let double = self.rail_press.is_some_and(|(t, q)| {
                    now.duration_since(t) <= dt && (q.x - p.x).abs() <= sx && (q.y - p.y).abs() <= sy
                });
                if double {
                    self.rail_press = None;
                    log::info!("chrome: rail double-click");
                    self.toggle_fullscreen();
                } else {
                    self.rail_press = Some((now, p));
                    if w.fullscreen().is_none() {
                        // Native move loop: Snap and snap layouts work as on a title bar.
                        log::info!("chrome: move");
                        if let Err(e) = w.drag_window() {
                            log::warn!("move: {e}");
                        }
                    }
                }
            }
            Hit::None => {}
        }
    }

    fn render(&mut self) {
        let why = self.pending.take().unwrap_or("system paint");
        let fading = self.tick();
        let view = View { palette: self.palette, link: smoothstep(self.link_t), close: self.close };
        let Some(live) = &mut self.live else { return };
        if self.hidden || self.minimized || live.config.width == 0 || live.config.height == 0 {
            return;
        }
        let Live { renderer, config, surface, gpu, window } = live;

        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(_) | wgpu::CurrentSurfaceTexture::Outdated => {
                surface.configure(&gpu.device, config);
                self.redraw("surface reconfigured");
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                log::warn!("surface lost; recreating");
                match gpu.instance.create_surface(window.clone()) {
                    Ok(s) => {
                        *surface = s;
                        surface.configure(&gpu.device, config);
                        self.redraw("surface recreated");
                    }
                    Err(e) => log::error!("surface recreate: {e}"),
                }
                return;
            }
            // Never retried in a loop: the next real change asks for a frame again.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                log::info!("frame skipped: surface timeout or occluded");
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                log::error!("surface validation error");
                return;
            }
        };

        let target = frame.texture.create_view(&Default::default());
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        renderer.draw(
            gpu,
            &mut encoder,
            &target,
            (config.width, config.height),
            window.scale_factor(),
            view,
        );
        gpu.queue.submit([encoder.finish()]);
        window.pre_present_notify();
        gpu.queue.present(frame);
        renderer.trim();
        let size = (config.width, config.height);

        self.frames += 1;
        if self.tracing() && !STEADY.contains(&why) {
            log::info!("+{} ms frame {}: {why}, {}x{}", self.ms(), self.frames, size.0, size.1);
        }
        match why {
            "idle present" => self.count(|a| a.idle_frames += 1),
            "cursor motion" => self.count(|a| a.motion_frames += 1),
            _ => self.count(|a| a.frames += 1),
        }
        if let Some(every) = self.idle_every {
            self.next_idle = Instant::now() + every;
        }

        if fading {
            self.redraw("fade");
        } else if self.motion_until.is_some_and(|t| Instant::now() < t) {
            self.redraw("cursor motion");
        } else {
            self.motion_until = None;
        }
    }

    /// Handles `Resized`. winit queues `WM_SIZE` raised inside a callback, so the event's
    /// size can be stale; the window's current client size is used instead.
    fn resize(&mut self) {
        let Some(live) = &self.live else { return };
        let size = live.window.inner_size();
        let minimized = size.width == 0 || size.height == 0;
        let resumed = !minimized && self.minimized;
        if minimized != self.minimized {
            log::info!(
                "suspend signal: Resized({}x{}) -> {}",
                size.width,
                size.height,
                if minimized { "suspended" } else { "resumed" }
            );
            self.minimized = minimized;
        }
        if minimized {
            return;
        }
        if self.centre_pending {
            self.centre();
        }
        let Some(live) = &mut self.live else { return };
        if (live.config.width, live.config.height) == (size.width, size.height) && !resumed {
            return;
        }
        live.config.width = size.width;
        live.config.height = size.height;
        live.surface.configure(&live.gpu.device, &live.config);
        self.redraw("resized");
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.live.is_some() {
            return;
        }
        let monitor = event_loop.primary_monitor().or_else(|| event_loop.available_monitors().next());
        let theme::Rgb(r, g, b) = theme::RULE_2;
        let windowed = self.opts.windowed;
        let mut attrs = Window::default_attributes()
            .with_title("PinnOS")
            .with_visible(false)
            .with_decorations(false)
            .with_undecorated_shadow(windowed)
            .with_border_color(Some(Color::from_rgb(r, g, b)));
        if let Some(m) = &monitor {
            let (mp, ms) = (m.position(), m.size());
            let (w, h) = (ms.width * 3 / 5, ms.height * 3 / 5);
            let pos = PhysicalPosition::new(mp.x + (ms.width - w) as i32 / 2, mp.y + (ms.height - h) as i32 / 2);
            self.centre_pending = windowed;
            attrs = attrs.with_inner_size(PhysicalSize::new(w, h)).with_position(pos);
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.error = Some(format!("create window: {e}"));
                event_loop.exit();
                return;
            }
        };
        set_chrome(&window, !windowed);
        if !windowed {
            window.set_fullscreen(Some(Fullscreen::Borderless(monitor)));
        }

        let (gpu, surface) = match Gpu::new(Some(window.clone())) {
            Ok((g, Some(s))) => (g, s),
            Ok((_, None)) => unreachable!("a window always yields a surface"),
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
                return;
            }
        };

        let caps = surface.get_capabilities(&gpu.adapter);
        let format = caps.formats.iter().copied().find(|f| render::is_target_format(*f)).unwrap_or_else(|| {
            log::warn!("no unorm 8-bit surface format in {:?}; colours may be off", caps.formats);
            caps.formats[0]
        });
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&gpu.device, &config);
        log::info!("window {}x{} scale {} format {format:?}", size.width, size.height, window.scale_factor());

        let renderer = Renderer::new(&gpu.device, &gpu.queue, format);
        self.live = Some(Live { renderer, config, surface, gpu, window: window.clone() });
        // A link that came up while the window was being made shows as up from the first
        // frame. Its queued `Up` event then changes nothing.
        if self.link.is_up() {
            self.link_up = true;
            self.link_t = 1.0;
        }
        self.last_tick = Instant::now();
        self.redraw("first frame");
        self.render();
        window.set_visible(true);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        let UiEvent::Link(event) = event;
        match event {
            LinkEvent::Up | LinkEvent::Down => {
                let up = matches!(event, LinkEvent::Up);
                if self.hidden && !up {
                    // The tray went with the core; nothing could bring the window back.
                    log::info!("link down while hidden; exiting");
                    event_loop.exit();
                    return;
                }
                if up != self.link_up {
                    self.link_up = up;
                    self.last_tick = Instant::now();
                    if self.minimized || self.hidden {
                        // No animation while unseen: snap to the new state.
                        self.link_t = if up { 1.0 } else { 0.0 };
                    }
                    self.redraw("link change");
                }
            }
            LinkEvent::Show => self.show(),
            LinkEvent::Exit => {
                log::info!("exit requested by core");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if self.tracing()
            && !matches!(
                event,
                WindowEvent::CursorMoved { .. } | WindowEvent::RedrawRequested | WindowEvent::AxisMotion { .. }
            )
        {
            log::info!("+{} ms event {event:?}", self.ms());
        }
        match event {
            WindowEvent::CloseRequested => self.close(event_loop, "close requested"),
            WindowEvent::Resized(_) => self.resize(),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                log::info!("scale factor {scale_factor}");
                self.redraw("scale factor");
            }
            WindowEvent::Occluded(occluded) => {
                log::info!("suspend signal: Occluded({occluded})");
                self.minimized = occluded;
                if !occluded {
                    self.redraw("unoccluded");
                }
            }
            WindowEvent::CursorMoved { position, .. } => self.cursor_moved(position),
            WindowEvent::CursorLeft { .. } => {
                self.last_move = None;
                if !self.close_armed {
                    self.set_close(Close::Rest);
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => self.left_button(event_loop, state),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed || event.repeat {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let (ctrl, alt) = (self.mods.control_key(), self.mods.alt_key());
                match code {
                    KeyCode::KeyQ if ctrl => self.close(event_loop, "Ctrl+Q"),
                    KeyCode::F4 if alt => self.close(event_loop, "Alt+F4"),
                    KeyCode::F11 => self.toggle_fullscreen(),
                    KeyCode::KeyP if !ctrl && !alt => {
                        self.palette = !self.palette;
                        log::info!("palette {}", if self.palette { "on" } else { "off" });
                        self.redraw("palette");
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.flush_activity(false);
        let mut wake = self.activity.since.map(|s| s + Duration::from_secs(1));
        if let Some(every) = self.idle_every.filter(|_| !self.hidden && !self.minimized) {
            let now = Instant::now();
            if now >= self.next_idle && self.pending.is_none() {
                self.redraw("idle present");
                self.next_idle = now + every;
            }
            wake = Some(wake.map_or(self.next_idle, |w| w.min(self.next_idle)));
        }
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.flush_activity(true);
        log::info!("exit; {} frames drawn", self.frames);
        self.live = None;
    }
}
