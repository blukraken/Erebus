//! The window. The knot animates whenever the window can be seen, paced by
//! `ControlFlow::WaitUntil` per render state:
//!
//! | State | When | Frames |
//! |---|---|---|
//! | Suspended | hidden to tray, minimised, or covered by the foreground window | none |
//! | Glance | visible, no input for [`GLANCE_AFTER`] | 30 fps cap, half the dots |
//! | Calm | visible, recent input | 60 fps cap, full detail |
//!
//! Fullscreen also presents on every cursor move; see [`MOTION_FRAMES`]. With Windows
//! animation effects off, the knot holds still and frames run only for a transition.
//! Other changes (resize, DPI, `P`, link, close button) redraw on demand as before.
//!
//! The animation clock advances only when a frame is drawn, by the real time since the
//! last one, capped at 100 ms. A state change never makes it jump.
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
//! Vitals samples arrive once a second and draw in the frames above; they never wake the
//! window on their own, except to redraw a visible window that is not animating. Samples
//! that arrive while Suspended are kept for the next frame.
//!
//! Every redraw request carries a reason. The first seconds after start or an F11 toggle
//! log each request and frame; `--trace` logs them all, plus per-second input counts.

use crate::feed::{self, Feed};
use crate::gpu::Gpu;
use crate::knot;
use crate::link::{self, Link, LinkEvent};
use crate::net::{self, Net};
use crate::panel::{self, Panel};
use crate::render::{self, Close, Renderer, Scene, View};
use crate::vitals::{self, Vitals};
use erebus_protocol::{Context, FaceMsg, GitState};
use erebus_theme as theme;
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
const STEADY: [&str; 3] = ["idle present", "cursor motion", "animate"];

/// Calm turns to Glance after this long without input.
const GLANCE_AFTER: Duration = Duration::from_secs(10);
const CALM_FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);
const GLANCE_FRAME: Duration = Duration::from_nanos(1_000_000_000 / 30);
/// How often the cover and animation-setting checks run while the window is visible.
const POLL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderState {
    Suspended,
    Glance,
    Calm,
}

/// Frames drawn in the current render state, for the transition log.
struct StateLog {
    state: RenderState,
    since: Instant,
    frames: u64,
}

pub struct Options {
    /// Start in the F11 window instead of fullscreen.
    pub windowed: bool,
    /// Log every redraw, and per-second input and frame counts, for the whole session.
    pub trace: bool,
    /// Present an unchanged frame at this rate while idle; 0 is off.
    pub idle_present_hz: f64,
    /// Present nothing extra while the cursor moves in fullscreen.
    pub no_motion_frames: bool,
    /// Hold the knot still as if Windows animation effects were off.
    pub reduced_motion: bool,
}

#[derive(Debug)]
pub enum UiEvent {
    Link(LinkEvent),
}

/// What the cursor is over.
#[derive(Clone, Debug, PartialEq)]
enum Hit {
    None,
    Close,
    /// The top rail: moves the window in windowed mode; a double-click toggles fullscreen.
    Rail,
    Edge(ResizeDirection),
    /// A vitals row: shows its tooltip, and a click opens the panel on it.
    Vital(vitals::Row),
    /// The open vitals panel, and its "— collapse" control.
    Panel,
    Collapse,
    /// The live feed: a row, the live chip, or the ground between rows.
    Feed(feed::Spot),
    /// A session's globe.
    Globe(String),
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
    anim_frames: u32,
    cursor_sets: u32,
    /// Main-thread CPU at `since`, and wall time spent in `render`, microseconds.
    cpu0: u64,
    render_us: u64,
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
    sim: knot::Sim,
    /// The core's last context. Kept while the link is down, so a reconnect can flare.
    context: Option<Context>,
    /// Project path and git state the last flare compared against.
    last_git: Option<(String, GitState)>,
    /// Wordmark crumbs and knot label for the current state.
    crumbs: Vec<String>,
    label: Option<String>,
    knot_hover: bool,
    /// Path and glyph centre of the last hovered file; kept while its label fades.
    file_label: Option<(String, [f32; 2])>,
    /// Windows animation effects are off, or `--reduced-motion`.
    reduced: bool,
    /// The foreground window covers this one.
    covered: bool,
    next_poll: Instant,
    last_input: Instant,
    /// Start of the last drawn frame; paces the next.
    last_frame: Instant,
    /// When the animation clock last advanced.
    last_sim: Instant,
    state: StateLog,
    vitals: Vitals,
    panel: Panel,
    collapse_hover: bool,
    feed: Feed,
    net: Net,
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
        sim: knot::Sim::default(),
        context: None,
        last_git: None,
        crumbs: Vec::new(),
        label: None,
        knot_hover: false,
        file_label: None,
        reduced: false,
        covered: false,
        next_poll: now,
        last_input: now,
        last_frame: now,
        last_sim: now,
        state: StateLog { state: RenderState::Calm, since: now, frames: 0 },
        vitals: Vitals::default(),
        panel: Panel::default(),
        collapse_hover: false,
        feed: Feed::new(),
        net: Net::new(),
    };
    app.reduced = app.opts.reduced_motion || animations_off();
    log::info!("reduced motion {}", if app.reduced { "on" } else { "off" });
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

fn iconic(w: &Window) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
    // SAFETY: query on this process's own live window.
    hwnd(w).is_some_and(|h| unsafe { IsIconic(h) } != 0)
}

/// CPU time of the calling thread, in microseconds.
fn thread_cpu_us() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    // SAFETY: the pseudo handle needs no closing; the FILETIMEs are ours.
    unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) / 10;
    t(k) + t(u)
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Windows "Animation effects" is off.
fn animations_off() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION};
    let mut on: i32 = 1;
    // SAFETY: the query writes one BOOL into `on`.
    let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&mut on as *mut i32).cast(), 0) };
    ok != 0 && on == 0
}

/// The foreground window is another app's and fully contains this one, so nothing of the
/// face can be seen: a fullscreen game, or a maximised window over the F11 window. The
/// desktop and taskbar never count.
fn covered(w: &Window) -> bool {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowRect, IsIconic, IsWindowVisible,
    };
    let Some(h) = hwnd(w) else { return false };
    // SAFETY: queries on live window handles into buffers owned here.
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() || fg == h || IsIconic(fg) != 0 || IsWindowVisible(fg) == 0 {
            return false;
        }
        let mut class = [0u16; 32];
        let n = GetClassNameW(fg, class.as_mut_ptr(), class.len() as i32);
        let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return false;
        }
        let mut a = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut b = a;
        GetWindowRect(fg, &mut a);
        GetWindowRect(h, &mut b);
        a.left <= b.left && a.top <= b.top && a.right >= b.right && a.bottom >= b.bottom
    }
}

/// `main · 348c109 · 2 changed`, or why there is no such line.
fn git_label(ctx: &Context) -> Option<String> {
    let project = ctx.project.as_ref()?;
    let Some(g) = &project.git else { return Some("no git".into()) };
    let branch = g.branch.as_deref().unwrap_or("detached");
    let head = g.head.as_deref().unwrap_or("no commits");
    let changed = if g.changed == 0 { "clean".to_string() } else { format!("{} changed", g.changed) };
    Some(format!("{branch} · {head} · {changed}"))
}

/// Wordmark crumbs and knot label for a context; none while the knot is dormant.
pub fn context_text(ctx: Option<&Context>, live: bool) -> (Vec<String>, Option<String>) {
    match ctx.filter(|_| live) {
        Some(c) => {
            let mut crumbs = vec![c.workspace.clone()];
            crumbs.extend(c.project.as_ref().map(|p| p.name.clone()));
            (crumbs, git_label(c))
        }
        None => (Vec::new(), None),
    }
}

/// The listed files and full changed count of a context.
pub fn context_files(ctx: Option<&Context>) -> (&[erebus_protocol::ChangedFile], u32) {
    match ctx.and_then(|c| c.project.as_ref()?.git.as_ref()) {
        Some(g) => (&g.files, g.changed),
        None => (&[], 0),
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
            "+{} ms activity over {} ms: {} moves, {} frames, {} idle frames, {} motion frames, {} anim frames, {} cursor sets, {:?}, main thread CPU {} us, render {} us",
            self.ms(),
            dt.as_millis(),
            a.moves,
            a.frames,
            a.idle_frames,
            a.motion_frames,
            a.anim_frames,
            a.cursor_sets,
            self.state.state,
            thread_cpu_us().saturating_sub(a.cpu0),
            a.render_us,
        );
    }

    fn knot_live(&self) -> bool {
        self.link_up && self.context.is_some()
    }

    /// Recomputes the knot's targets and words from link, context and hover.
    fn refresh_knot(&mut self) {
        let live = self.knot_live();
        let (crumbs, label) = context_text(self.context.as_ref(), live);
        self.crumbs = crumbs;
        self.label = label;
        self.sim.live_target = if live { 1.0 } else { 0.0 };
        self.sim.hover_target = if self.knot_hover && self.label.is_some() { 1.0 } else { 0.0 };
    }

    fn set_context(&mut self, ctx: Context) {
        let project = ctx.project.as_ref();
        let git = project.and_then(|p| Some((p.path.clone(), p.git.clone()?)));
        let seen = self.state.state != RenderState::Suspended;
        let mut commit = None;
        if let (Some((old_path, old)), Some((path, new))) = (&self.last_git, &git) {
            // A switch to another project is not a git change, so it does not flare.
            if old_path == path {
                if let Some(f) = knot::flare_for(old, new) {
                    if seen {
                        self.sim.flare(f);
                    }
                    log::info!("context: {f:?} change{}", if seen { "; flare" } else { " while suspended; no flare" });
                }
                // A new head on the same branch is a commit: files that left it dash home.
                if old.branch == new.branch && old.head != new.head {
                    commit = new.head.clone();
                }
            }
        }
        self.last_git = git;
        self.context = Some(ctx);
        self.refresh_knot();

        let dash = seen && !self.reduced;
        let (list, changed) = context_files(self.context.as_ref());
        let c = self.sim.set_files(list, changed, commit.as_deref(), dash);
        log::info!(
            "files: {} listed of {changed}; {} new, {} dash, {} fade{}",
            list.len(),
            c.added,
            c.dashed,
            c.faded,
            c.first_arrival.map_or(String::new(), |t| format!("; first arrival in {:.0} ms", t * 1000.0)),
        );
        if let Some([x, y]) = self.sim.files.first().and_then(|i| self.glyph_screen(i)) {
            log::info!("files: first glyph at {}, {}", x.round(), y.round());
        }
        if !seen {
            self.sim.snap();
        }
        self.redraw("context");
    }

    fn render_state(&self, now: Instant) -> RenderState {
        if self.hidden || self.minimized || self.covered || self.live.is_none() {
            RenderState::Suspended
        } else if now.duration_since(self.last_input) >= GLANCE_AFTER {
            RenderState::Glance
        } else {
            RenderState::Calm
        }
    }

    /// Moves to the render state that applies now, and logs the frames the last one drew.
    fn update_state(&mut self, now: Instant) {
        let next = self.render_state(now);
        let prev = self.state.state;
        if next == prev {
            return;
        }
        let secs = now.duration_since(self.state.since).as_secs_f64();
        let frames = self.state.frames;
        log::info!(
            "render state {prev:?} -> {next:?} after {secs:.1} s: {frames} frames ({:.1} fps)",
            frames as f64 / secs.max(1e-3)
        );
        self.state = StateLog { state: next, since: now, frames: 0 };
        self.sim.lod_target = if next == RenderState::Glance { 0.0 } else { 1.0 };
        if prev == RenderState::Suspended {
            // Missed events do not replay: the knot resumes at the current state.
            self.sim.snap();
            self.vitals.snap();
            self.panel.snap();
            self.feed.snap();
            self.net.snap();
            self.last_sim = now;
            self.redraw("resumed");
        }
    }

    fn input(&mut self) {
        self.last_input = Instant::now();
    }

    /// Where glyph `i` is drawn now, in client pixels.
    fn glyph_screen(&self, i: usize) -> Option<[f32; 2]> {
        let live = self.live.as_ref()?;
        let (w, h) = (live.config.width, live.config.height);
        let r = knot::radius(w, h, live.window.scale_factor());
        Some(self.sim.files.screen(i, [w as f32 * 0.5, h as f32 * 0.5], r, self.sim.rot()))
    }

    /// The listed file glyph under the cursor, if the knot is live.
    fn file_hit(&self, p: PhysicalPosition<f64>) -> Option<(usize, String)> {
        let live = self.live.as_ref().filter(|_| self.knot_live())?;
        let (w, h) = (live.config.width, live.config.height);
        let r = knot::radius(w, h, live.window.scale_factor());
        let centre = [w as f32 * 0.5, h as f32 * 0.5];
        let (i, path) = self.sim.files.hit([p.x as f32, p.y as f32], centre, r, self.sim.rot(), r * 0.06)?;
        Some((i, path.to_string()))
    }

    /// Animation frames are due: the knot moves, or a transition runs.
    fn animating(&self) -> bool {
        self.state.state != RenderState::Suspended && (!self.reduced || self.sim.busy() || self.panel.busy() || self.feed.busy() || self.net.busy())
    }

    /// Hover hit on the knot: inside 0.9 of its radius, as in the prototype.
    fn knot_hit(&self, p: PhysicalPosition<f64>) -> bool {
        let Some(live) = &self.live else { return false };
        let (w, h) = (live.config.width, live.config.height);
        let r = knot::radius(w, h, live.window.scale_factor()) as f64 * 0.9;
        let (dx, dy) = (p.x - w as f64 * 0.5, p.y - h as f64 * 0.5);
        dx * dx + dy * dy < r * r
    }

    fn count(&mut self, f: impl FnOnce(&mut Activity)) {
        if self.opts.trace {
            if self.activity.since.is_none() {
                self.activity.since = Some(Instant::now());
                self.activity.cpu0 = thread_cpu_us();
            }
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
            return Hit::Close;
        } else if y < c.rail {
            return Hit::Rail;
        }
        let Some(col) = vitals::column(cw, ch, scale).filter(|_| self.vitals.has_data()) else { return Hit::None };
        if let Some(hw) = self.vitals.hardware().filter(|_| self.panel.is_open()) {
            let l = panel::layout(&col, hw.cores as usize, (cw, ch));
            if l.over_collapse(x, y) {
                return Hit::Collapse;
            } else if l.contains(x, y) {
                return Hit::Panel;
            }
        }
        if let Some(spot) = self.feed.hit(&col, x, y) {
            return Hit::Feed(spot);
        }
        let area = net::Area::new(&col, cw, ch, knot::radius(cw, ch, scale));
        if let Some(id) = self.net.hit(&area, x, y) {
            return Hit::Globe(id);
        }
        col.hit(x, y).map_or(Hit::None, Hit::Vital)
    }

    /// Scrolls the feed when the cursor is over it: a notch is one row, a touchpad moves by
    /// pixels.
    fn wheel(&mut self, delta: winit::event::MouseScrollDelta) {
        let Some(p) = self.cursor else { return };
        if !matches!(self.hit(p), Hit::Feed(_)) {
            return;
        }
        let Some(live) = &self.live else { return };
        let Some(col) = vitals::column(live.config.width, live.config.height, live.window.scale_factor()) else { return };
        let rows = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, y) => y,
            winit::event::MouseScrollDelta::PixelDelta(d) => d.y as f32 / feed::layout(&col).pitch,
        };
        self.feed.scroll_by(rows);
        self.redraw("feed scroll");
    }

    /// Opens, moves or closes the vitals panel. A window that is not animating starts at once.
    fn panel_change(&mut self, change: impl FnOnce(&mut Panel)) {
        let before = (self.panel.is_open(), self.panel.focus());
        change(&mut self.panel);
        if (self.panel.is_open(), self.panel.focus()) == before {
            return;
        }
        match self.panel.focus() {
            Some(row) => log::info!("vitals panel on {row:?}"),
            None => log::info!("vitals panel closed"),
        }
        if self.panel.is_open() {
            self.set_vital_hover(None);
        }
        self.redraw("vitals panel");
    }

    /// Shows the hovered row's tooltip. A window that is not animating draws it at once.
    fn set_vital_hover(&mut self, row: Option<vitals::Row>) {
        if row == self.vitals.hover {
            return;
        }
        self.vitals.hover = row;
        log::info!("vitals hover {row:?}");
        if !self.animating() {
            self.redraw("vitals hover");
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
        if moved {
            self.input();
        }
        let over_file = self.file_hit(p);
        if over_file.as_ref().map(|f| f.0) != self.sim.hovered {
            self.sim.hovered = over_file.as_ref().map(|f| f.0);
            if let Some((i, path)) = over_file {
                log::info!("file hover in: {path}");
                self.file_label = self.glyph_screen(i).map(|at| (path, at));
            } else {
                log::info!("file hover out");
            }
            if self.sim.busy() && self.pending.is_none() {
                self.redraw("file hover");
            }
        }
        let over_knot = self.knot_hit(p);
        if over_knot != self.knot_hover {
            self.knot_hover = over_knot;
            self.refresh_knot();
            log::info!("knot hover {}, label {:?}", if over_knot { "in" } else { "out" }, self.label);
            if self.sim.busy() && self.pending.is_none() {
                self.redraw("knot hover");
            }
        }
        let fullscreen = self.live.as_ref().is_some_and(|l| l.window.fullscreen().is_some());
        if moved && fullscreen && !self.opts.no_motion_frames {
            self.motion_until = Some(Instant::now() + MOTION_FRAMES);
            if self.pending.is_none() {
                self.redraw("cursor motion");
            }
        }
        self.cursor = Some(p);
        let hit = self.hit(p);
        let globe = if let Hit::Globe(id) = &hit { Some(id.clone()) } else { None };
        if globe != self.net.hover {
            self.net.hover = globe;
            self.redraw("globe hover");
        }
        let row = if let Hit::Feed(feed::Spot::Row(id)) = hit { Some(id) } else { None };
        let chip = matches!(hit, Hit::Feed(feed::Spot::Chip));
        if row != self.feed.hover || chip != self.feed.chip_hover {
            self.feed.hover = row;
            self.feed.chip_hover = chip;
            self.redraw("feed hover");
        }
        let tip = if let Hit::Vital(r) = hit { (!self.panel.is_open()).then_some(r) } else { None };
        self.set_vital_hover(tip);
        if (hit == Hit::Collapse) != self.collapse_hover {
            self.collapse_hover = hit == Hit::Collapse;
            self.redraw("collapse hover");
        }
        let over = hit == Hit::Close;
        self.set_close(match (self.close_armed, over) {
            (true, true) => Close::Press,
            (false, true) => Close::Hover,
            _ => Close::Rest,
        });
        let icon = match hit {
            Hit::Edge(d) => resize_cursor(d),
            Hit::Vital(_) | Hit::Collapse | Hit::Feed(feed::Spot::Chip) => CursorIcon::Pointer,
            _ => CursorIcon::Default,
        };
        if icon != self.cursor_icon {
            self.cursor_icon = icon;
            self.count(|a| a.cursor_sets += 1);
            if let Some(live) = &self.live {
                live.window.set_cursor(icon);
            }
        }
    }

    fn left_button(&mut self, event_loop: &ActiveEventLoop, state: ElementState) {
        self.input();
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
        if self.panel.is_open() && !matches!(hit, Hit::Panel | Hit::Collapse | Hit::Vital(_)) {
            self.panel_change(Panel::close);
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
            Hit::Vital(row) => self.panel_change(|p| p.toggle(row)),
            Hit::Collapse => self.panel_change(Panel::close),
            Hit::Feed(feed::Spot::Chip) => {
                log::info!("feed: back to live");
                self.feed.go_live();
                self.redraw("feed live");
            }
            Hit::Feed(feed::Spot::Row(id)) => {
                if let Some(s) = self.feed.session_of(id).map(str::to_string) {
                    log::info!("feed: ping {s}");
                    self.net.ping(&s);
                    self.redraw("ping");
                }
            }
            Hit::Panel | Hit::Feed(_) | Hit::Globe(_) | Hit::None => {}
        }
    }

    fn render(&mut self) {
        let why = self.pending.take().unwrap_or("system paint");
        let fading = self.tick();
        let now = Instant::now();
        let t0 = now;
        self.update_state(now);
        let dt = now.duration_since(self.last_sim).as_secs_f32();
        self.last_sim = now;
        self.sim.step(dt, self.reduced);
        self.net.step(dt, self.reduced, unix_ms());
        self.vitals.step(dt, self.reduced);
        self.panel.step(dt, self.reduced);
        self.feed.step(dt, self.reduced);
        let view = View { palette: self.palette, link: smoothstep(self.link_t), close: self.close };
        let more = if self.knot_live() { self.sim.files.more() } else { None };
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
        let (w, h, scale) = (config.width, config.height, window.scale_factor());
        let crumbs: Vec<&str> = self.crumbs.iter().map(String::as_str).collect();
        let column = vitals::column(w, h, scale);
        let vitals = self.vitals.frame(column.as_ref(), unix_ms(), (w, h), self.reduced);
        let area = column.as_ref().map(|c| net::Area::new(c, w, h, knot::radius(w, h, scale)));
        if let Some(a) = area {
            self.net.set_area(a);
        }
        let net = area.as_ref().and_then(|a| self.net.frame(a, self.reduced));
        // A globe's name chip shares the file label's box when no file is hovered.
        let globe_label = net.as_ref().and_then(|n| n.label.as_ref()).filter(|_| self.sim.hovered.is_none());
        let panel = column.as_ref().and_then(|c| panel::frame(&self.panel, &self.vitals, c, (w, h), self.collapse_hover, self.reduced));
        let feed = column.as_ref().and_then(|c| self.feed.frame(c, self.reduced));
        let scene = Scene {
            view,
            knot: self.sim.uniform(w, h, scale, self.reduced),
            crumbs: &crumbs,
            label: self.label.as_deref(),
            label_alpha: self.sim.hover(),
            files: self.sim.files.instances(),
            files_gen: self.sim.files.generation,
            file_label: match globe_label {
                Some(l) => Some((l.text.as_str(), l.at)),
                None => self.file_label.as_ref().map(|(p, at)| (p.as_str(), *at)),
            },
            file_label_alpha: globe_label.map_or(self.sim.file_hover(), |l| l.alpha),
            more: more.as_deref(),
            vitals: vitals.as_ref(),
            panel: panel.as_ref(),
            feed: feed.as_ref(),
            net: net.as_ref(),
        };
        // Wraps on whole turns of the core's jitter, which runs at three radians a second.
        renderer.net_clock = (renderer.net_clock + dt) % (std::f32::consts::TAU / 3.0 * 1000.0);
        renderer.draw(gpu, &mut encoder, &target, (w, h), scale, &scene);
        gpu.queue.submit([encoder.finish()]);
        window.pre_present_notify();
        gpu.queue.present(frame);
        renderer.trim();
        let size = (config.width, config.height);

        let spent = t0.elapsed().as_micros() as u64;
        self.count(|a| a.render_us += spent);
        self.frames += 1;
        self.state.frames += 1;
        self.last_frame = now;
        if self.tracing() && !STEADY.contains(&why) {
            log::info!("+{} ms frame {}: {why}, {}x{}", self.ms(), self.frames, size.0, size.1);
        }
        match why {
            "idle present" => self.count(|a| a.idle_frames += 1),
            "cursor motion" => self.count(|a| a.motion_frames += 1),
            "animate" => self.count(|a| a.anim_frames += 1),
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
        // A borderless window minimises to a 160x28 parked frame, not 0x0.
        let minimized = size.width == 0 || size.height == 0 || iconic(&live.window);
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
            .with_title("Erebus")
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
                if !up {
                    self.vitals.clear();
                    self.panel = Panel::default();
                    self.feed.clear();
                    self.net.clear();
                }
                if up != self.link_up {
                    self.link_up = up;
                    self.last_tick = Instant::now();
                    self.refresh_knot();
                    if self.minimized || self.hidden {
                        // No animation while unseen: snap to the new state.
                        self.link_t = if up { 1.0 } else { 0.0 };
                        self.sim.snap();
                    }
                    self.redraw("link change");
                }
            }
            LinkEvent::Context(ctx) => self.set_context(*ctx),
            LinkEvent::Vitals(v) => {
                let first = !self.vitals.has_data();
                let (hw, n) = (v.hw.clone(), v.samples.len());
                let added = self.vitals.ingest(*v);
                if first {
                    log::info!("vitals: {added} of {n} samples, {hw:?}");
                }
                // Animating frames pick the sample up; Suspended keeps it for later.
                if self.state.state != RenderState::Suspended && !self.animating() {
                    self.redraw("vitals");
                }
            }
            LinkEvent::Sessions(sessions) => {
                let workers: usize = sessions.iter().map(|s| s.workers.len()).sum();
                log::info!("sessions: {} live, {workers} workers", sessions.len());
                self.net.ingest_sessions(&sessions, unix_ms());
                if self.state.state != RenderState::Suspended && !self.animating() {
                    self.redraw("sessions");
                }
            }
            LinkEvent::Feed(events) => {
                self.net.note_activity(&events);
                self.feed.ingest(events);
                if self.state.state != RenderState::Suspended && !self.animating() {
                    self.redraw("feed");
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
                self.set_vital_hover(None);
                if !self.close_armed {
                    self.set_close(Close::Rest);
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => self.left_button(event_loop, state),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::MouseWheel { delta, .. } => {
                self.input();
                self.wheel(delta);
            }
            WindowEvent::MouseInput { .. } => self.input(),
            WindowEvent::KeyboardInput { event, .. } => {
                self.input();
                if event.state != ElementState::Pressed || event.repeat {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let (ctrl, alt) = (self.mods.control_key(), self.mods.alt_key());
                match code {
                    KeyCode::KeyQ if ctrl => self.close(event_loop, "Ctrl+Q"),
                    KeyCode::F4 if alt => self.close(event_loop, "Alt+F4"),
                    KeyCode::F11 => self.toggle_fullscreen(),
                    KeyCode::Escape => self.panel_change(Panel::close),
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
        let now = Instant::now();
        let mut wake = self.activity.since.map(|s| s + Duration::from_secs(1));
        let mut at = |t: Instant| wake = Some(wake.map_or(t, |w: Instant| w.min(t)));

        // Cover and animation-setting checks, only while the window could be seen.
        let shown = !self.hidden && !self.minimized;
        if shown && now >= self.next_poll {
            self.next_poll = now + POLL;
            let covered = self.live.as_ref().is_some_and(|l| covered(&l.window));
            if covered != self.covered {
                log::info!("{}", if covered { "covered by the foreground window" } else { "uncovered" });
                self.covered = covered;
            }
            let reduced = self.opts.reduced_motion || animations_off();
            if reduced != self.reduced {
                log::info!("reduced motion {}", if reduced { "on" } else { "off" });
                self.reduced = reduced;
            }
        }
        self.update_state(now);

        if self.animating() {
            let every = if self.state.state == RenderState::Glance { GLANCE_FRAME } else { CALM_FRAME };
            let next = self.last_frame + every;
            if now >= next {
                if self.pending.is_none() {
                    self.redraw("animate");
                }
            } else {
                at(next);
            }
        }
        if shown {
            at(self.next_poll);
        }
        if self.state.state == RenderState::Calm {
            at(self.last_input + GLANCE_AFTER);
        }

        if let Some(every) = self.idle_every.filter(|_| shown) {
            if now >= self.next_idle && self.pending.is_none() {
                self.redraw("idle present");
                self.next_idle = now + every;
            }
            at(self.next_idle);
        }
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.flush_activity(true);
        log::info!("exit; {} frames drawn", self.frames);
        self.live = None;
    }
}
