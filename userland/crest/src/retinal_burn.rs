use slope::SyscallError;
use slope::process::tachyon;

use crate::manifold::{DisplayBackend, DisplayError, DisplayManifold};

pub const MAXIMUM_LINES_PER_SLICE: u32 = 64;

pub trait ScanlineRenderer {
    type Error;

    fn render_line(&mut self, line: u32, width: u32) -> Result<(), Self::Error>;
}

pub struct BeamRaceReport {
    pub first_line: u32,
    pub rendered_lines: u32,
    pub yield_result: Result<(), SyscallError>,
}

/// Renders one bounded region ahead of the current beam position.
pub fn execute_beam_slice<B: DisplayBackend, R: ScanlineRenderer>(
    manifold: &DisplayManifold<B>,
    lead_lines: u32,
    requested_lines: u32,
    renderer: &mut R,
) -> Result<BeamRaceReport, BeamRaceError<R::Error>> {
    let mode = manifold.mode();
    if lead_lines >= mode.height || requested_lines == 0 {
        return Err(BeamRaceError::InvalidSlice);
    }
    let rendered_lines = requested_lines
        .min(MAXIMUM_LINES_PER_SLICE)
        .min(mode.height);
    let beam = manifold
        .get_beam_position()
        .map_err(BeamRaceError::Display)?;
    let first_line = (beam + lead_lines) % mode.height;
    for offset in 0..rendered_lines {
        let line = (first_line + offset) % mode.height;
        renderer
            .render_line(line, mode.width)
            .map_err(BeamRaceError::Renderer)?;
    }
    Ok(BeamRaceReport {
        first_line,
        rendered_lines,
        yield_result: tachyon::yield_retrocausally(u64::from(rendered_lines)),
    })
}

#[derive(Debug, Eq, PartialEq)]
pub enum BeamRaceError<RendererError> {
    InvalidSlice,
    Display(DisplayError),
    Renderer(RendererError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifold::{DisplayMode, PixelFormat, PresentFence};

    struct Backend;
    impl DisplayBackend for Backend {
        fn beam_position(&self) -> Result<u32, DisplayError> {
            Ok(100)
        }

        fn present(
            &mut self,
            _framebuffer: crate::manifold::FramebufferLease,
        ) -> Result<PresentFence, DisplayError> {
            Ok(PresentFence { sequence: 1 })
        }
    }

    struct Renderer(u32);
    impl ScanlineRenderer for Renderer {
        type Error = ();

        fn render_line(&mut self, _line: u32, _width: u32) -> Result<(), Self::Error> {
            self.0 += 1;
            Ok(())
        }
    }

    #[test]
    fn caps_each_beam_race_to_a_bounded_slice() {
        let mode = DisplayMode::new(1920, 1080, 7680, PixelFormat::Argb8888).unwrap();
        let manifold = DisplayManifold::new(mode, Backend);
        let mut renderer = Renderer(0);
        let report = execute_beam_slice(&manifold, 10, 500, &mut renderer).unwrap();
        assert_eq!(report.first_line, 110);
        assert_eq!(report.rendered_lines, MAXIMUM_LINES_PER_SLICE);
        assert_eq!(renderer.0, MAXIMUM_LINES_PER_SLICE);
    }
}
