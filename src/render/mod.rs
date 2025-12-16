pub mod egl;
pub mod surface;

pub use egl::EglContext;
pub use surface::MirrorSurface;

#[derive(Debug, Clone, Copy, Default)]
pub enum ScaleMode {
    /// Preserve aspect ratio, fit within target (letterbox/pillarbox)
    #[default]
    Fit,
    /// Preserve aspect ratio, fill target completely (crops edges)
    Fill,
    /// Stretch to fill target, ignoring aspect ratio
    Stretch,
    /// Display at 1:1 pixel ratio, centered (no scaling)
    Center,
}
