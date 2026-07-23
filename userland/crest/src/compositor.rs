#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceId(pub u64);
pub mod gesture;
pub mod panel;
pub mod pipeline;
pub mod riemann;
pub mod settings;
pub mod swapchain;
pub mod tween;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rectangle {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
