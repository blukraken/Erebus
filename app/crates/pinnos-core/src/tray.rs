//! Notification-area icon and its menu. Runs on its own thread with a Win32 message loop,
//! separate from the tokio runtime, and talks to the hub through its event channel.

use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE};
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, PostMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem,
    SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, CREATESTRUCTW, GWLP_USERDATA, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MSG, SM_CXSMICON, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_LBUTTONDBLCLK, WM_NCCREATE, WM_NULL, WM_RBUTTONUP, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::hub::Event;
use crate::icon;
use crate::ids::{ID_CLOSE, ID_OPEN, ID_QUIT, TRAY_CLASS};
use crate::log::{log_error, log_info, log_warn};
use crate::route::TrayAction;

const TIP: &str = "PinnOS";

/// Shell callback message for icon events.
const WM_TRAY: u32 = WM_APP + 1;
/// Posted by the hub; `wparam` is the number of connected faces.
const WM_FACES: u32 = WM_APP + 2;

const ICON_ID: u32 = 1;
const START_WAIT: Duration = Duration::from_secs(5);

/// The running tray thread.
pub struct Tray {
    hwnd: usize,
    thread: JoinHandle<()>,
}

impl Tray {
    pub fn hwnd(&self) -> usize {
        self.hwnd
    }

    /// Closes the tray window, which removes the icon, and waits for the thread to end.
    pub fn shutdown(self) {
        unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
        let _ = self.thread.join();
    }
}

/// Tells the tray how many faces are connected, so it can grey out "Close window".
pub fn faces_changed(hwnd: usize, count: usize) {
    unsafe { PostMessageW(hwnd as HWND, WM_FACES, count, 0) };
}

/// Starts the tray thread. `None` when the window cannot be created.
pub fn spawn(hub: UnboundedSender<Event>) -> Option<Tray> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = thread::Builder::new().name("tray".into()).spawn(move || thread_main(hub, ready_tx)).ok()?;
    match ready_rx.recv_timeout(START_WAIT) {
        Ok(Some(hwnd)) => Some(Tray { hwnd, thread }),
        _ => {
            log_error!("tray window was not created");
            None
        }
    }
}

struct State {
    hub: UnboundedSender<Event>,
    faces: usize,
    taskbar_created: u32,
    icon: isize,
}

fn thread_main(hub: UnboundedSender<Event>, ready: mpsc::Sender<Option<usize>>) {
    unsafe {
        // System DPI awareness makes SM_CXSMICON report the real tray icon size.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);

        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        let mut state = State { hub, faces: 0, taskbar_created, icon: 0 };

        let hinst = GetModuleHandleW(null());
        let class = wide(TRAY_CLASS);
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        if RegisterClassExW(&wc) == 0 {
            log_error!("tray class registration failed: {}", std::io::Error::last_os_error());
            let _ = ready.send(None);
            return;
        }

        // A hidden top-level window (not message-only) so it receives the TaskbarCreated broadcast.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            (&mut state as *mut State).cast(),
        );
        if hwnd.is_null() {
            log_error!("tray window creation failed: {}", std::io::Error::last_os_error());
            let _ = ready.send(None);
            return;
        }
        state.add_icon(hwnd);
        let _ = ready.send(Some(hwnd as usize));

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if state.icon != 0 {
            DestroyIcon(state.icon as _);
        }
    }
}

impl State {
    /// Adds the icon to the notification area, replacing any icon handle from an earlier add.
    unsafe fn add_icon(&mut self, hwnd: HWND) {
        if self.icon != 0 {
            DestroyIcon(self.icon as _);
        }
        let px = pick_size();
        let Some(hicon) = make_icon(px) else {
            log_error!("tray icon creation failed: {}", std::io::Error::last_os_error());
            self.icon = 0;
            return;
        };
        self.icon = hicon as isize;

        let mut nid = nid(hwnd);
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = hicon;
        for (dst, src) in nid.szTip.iter_mut().zip(TIP.encode_utf16()) {
            *dst = src;
        }
        if Shell_NotifyIconW(NIM_ADD, &nid) != 0 {
            log_info!("Shell_NotifyIconW NIM_ADD ok; {px} px icon");
        } else {
            log_warn!("Shell_NotifyIconW NIM_ADD failed: {}", std::io::Error::last_os_error());
        }
    }

    fn act(&self, action: TrayAction) {
        let _ = self.hub.send(Event::Tray(action));
    }
}

unsafe fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = ICON_ID;
    nid
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lp as *const CREATESTRUCTW;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    let Some(state) = state.as_mut() else { return DefWindowProcW(hwnd, msg, wp, lp) };

    match msg {
        WM_TRAY => match (lp & 0xFFFF) as u32 {
            WM_RBUTTONUP => show_menu(hwnd, state.faces > 0),
            WM_LBUTTONDBLCLK => state.act(TrayAction::Open),
            _ => {}
        },
        WM_COMMAND => match wp & 0xFFFF {
            ID_OPEN => state.act(TrayAction::Open),
            ID_CLOSE => state.act(TrayAction::Close),
            ID_QUIT => state.act(TrayAction::Quit),
            _ => {}
        },
        WM_FACES => state.faces = wp,
        WM_DESTROY => {
            Shell_NotifyIconW(NIM_DELETE, &nid(hwnd));
            log_info!("tray icon removed");
            PostQuitMessage(0);
        }
        m if m == state.taskbar_created => {
            log_info!("explorer restarted; re-adding tray icon");
            state.add_icon(hwnd);
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

/// The menu posts `WM_COMMAND` back to the window, the same path the tests drive.
unsafe fn show_menu(hwnd: HWND, face_connected: bool) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let close_flags = if face_connected { MF_STRING } else { MF_STRING | MF_GRAYED };
    AppendMenuW(menu, MF_STRING, ID_OPEN, wide("Open PinnOS").as_ptr());
    AppendMenuW(menu, close_flags, ID_CLOSE, wide("Close window").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, ID_QUIT, wide("Quit PinnOS").as_ptr());
    SetMenuDefaultItem(menu, ID_OPEN as u32, 0);

    let mut at: POINT = zeroed();
    GetCursorPos(&mut at);
    // The menu only dismisses on an outside click if the owner is foreground.
    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN, at.x, at.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

/// The generated size closest above the shell's small-icon size, else the largest.
fn pick_size() -> u32 {
    let want = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(0) as u32;
    icon::SIZES.into_iter().find(|&s| s >= want).unwrap_or(icon::SIZES[icon::SIZES.len() - 1])
}

fn make_icon(px: u32) -> Option<windows_sys::Win32::UI::WindowsAndMessaging::HICON> {
    let mut bgra = icon::render(px);
    bgra.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p.swap(0, 2));
    // The AND mask is ignored for 32-bit icons with alpha; zeros keep every pixel.
    let mask = vec![0u8; (px.div_ceil(16) * 2 * px) as usize];
    let hicon = unsafe { CreateIcon(null_mut(), px as i32, px as i32, 1, 32, mask.as_ptr(), bgra.as_ptr()) };
    (!hicon.is_null()).then_some(hicon)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
