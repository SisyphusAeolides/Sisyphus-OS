#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEvent {
    Key { code: u32, pressed: bool },
    PointerMotion { delta_x: i32, delta_y: i32 },
    PointerButton { button: u32, pressed: bool },
}
