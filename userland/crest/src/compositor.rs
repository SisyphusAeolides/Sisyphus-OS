#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceId(pub u64);
pub mod riemann;
pub mod pipeline;
pub mod tween;
pub mod gesture;
pub mod panel;
pub mod settings;
pub mod swapchain;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rectangle {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
