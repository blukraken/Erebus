//! Maps a tray action and the connected faces to the effects the hub performs.

use erebus_protocol::CoreMsg;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    Close,
    Quit,
}

#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Deliver a message to the face with this client id.
    Send(u64, CoreMsg),
    /// Launch `erebus-face.exe`.
    SpawnFace,
    /// Stop the core after the sends above are flushed.
    Shutdown,
}

/// `faces` holds the ids of the connected faces in connection order.
pub fn plan(action: TrayAction, faces: &[u64]) -> Vec<Effect> {
    let to_all = |msg: CoreMsg| faces.iter().map(move |&id| Effect::Send(id, msg.clone()));
    match action {
        TrayAction::Open if faces.is_empty() => vec![Effect::SpawnFace],
        TrayAction::Open => to_all(CoreMsg::Show).collect(),
        TrayAction::Close => to_all(CoreMsg::Exit).collect(),
        TrayAction::Quit => to_all(CoreMsg::Exit).chain([Effect::Shutdown]).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Effect::*;
    use TrayAction::*;

    #[test]
    fn no_face() {
        assert_eq!(plan(Open, &[]), [SpawnFace]);
        assert_eq!(plan(Close, &[]), []);
    }

    #[test]
    fn one_face() {
        assert_eq!(plan(Open, &[4]), [Send(4, CoreMsg::Show)]);
        assert_eq!(plan(Close, &[4]), [Send(4, CoreMsg::Exit)]);
    }

    #[test]
    fn two_faces() {
        assert_eq!(plan(Open, &[2, 5]), [Send(2, CoreMsg::Show), Send(5, CoreMsg::Show)]);
        assert_eq!(plan(Close, &[2, 5]), [Send(2, CoreMsg::Exit), Send(5, CoreMsg::Exit)]);
    }

    #[test]
    fn quit_exits_every_face_then_shuts_down() {
        assert_eq!(plan(Quit, &[]), [Shutdown]);
        assert_eq!(plan(Quit, &[1]), [Send(1, CoreMsg::Exit), Shutdown]);
        assert_eq!(
            plan(Quit, &[1, 3]),
            [Send(1, CoreMsg::Exit), Send(3, CoreMsg::Exit), Shutdown]
        );
    }
}
