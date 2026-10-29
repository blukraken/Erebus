//! Tray menu command ids and the window class. Shared with the integration tests.

/// Window class of the hidden tray window.
pub const TRAY_CLASS: &str = "PinnOSTray";

/// `WM_COMMAND` id of "Open PinnOS".
pub const ID_OPEN: usize = 1001;
/// `WM_COMMAND` id of "Close window".
pub const ID_CLOSE: usize = 1002;
/// `WM_COMMAND` id of "Quit PinnOS".
pub const ID_QUIT: usize = 1003;
