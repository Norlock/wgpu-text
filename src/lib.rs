//! wgpu-text is a wrapper over [glyph-brush](https://github.com/alexheretic/glyph-brush)
//! for simpler text rendering in [wgpu](https://github.com/gfx-rs/wgpu).
//!
//! This project was inspired by and is similar to [wgpu_glyph](https://github.com/hecrj/wgpu_glyph),
//! but has additional features and is simpler. Also there is no need to include glyph-brush in your project.
//!
//! Some features are directly implemented from glyph-brush so you should go trough
//! [Section docs](https://docs.rs/glyph_brush/latest/glyph_brush/struct.Section.html)
//! for better understanding of adding and managing text.
//!
//! * Look trough [examples](https://github.com/Blatko1/wgpu_text/tree/master/examples).

mod cache;
mod pipeline;

/// Contains all needed structs and enums for inserting and styling text. Directly taken from glyph_brush.
///
/// Look into [glyph_brush_layout docs](https://docs.rs/glyph_brush_layout/latest/glyph_brush_layout/#enums)
/// for the real, detailed, documentation.
/// - If anything is missing create an issue on github and I'll add it.
pub mod section {
    #[doc(hidden)]
    pub use glyph_brush::{
        BuiltInLineBreaker, Color, FontId, HorizontalAlign, Layout, LineBreak, OwnedSection,
        OwnedText, Section, SectionText, Text, VerticalAlign,
    };
}

use glyph_brush::{
    ab_glyph::{Font, FontArc, FontRef, InvalidFont},
    BrushAction, DefaultSectionHasher, Extra, Section,
};
use pipeline::{Pipeline, Vertex};

/// Marks scissor region and can test how to fit itself inside the specified window dimensions
/// to avoid `wgpu` related rendering errors.
pub struct ScissorRegion {
    /// x coordinate of top left region point.
    pub x: u32,

    /// y coordinate of top left region point.
    pub y: u32,

    /// Width of scissor region.
    pub width: u32,

    /// Height of scissor region.
    pub height: u32,
}

impl ScissorRegion {
    /// Checks if the region is contained in surface bounds at all.
    pub(crate) fn is_contained_in(&self, width: u32, height: u32) -> bool {
        if self.x < width && self.y < height {
            return true;
        }
        false
    }

    /// Gives available bounds paying attention to `s_width` and `s_height`.
    pub(crate) fn available_bounds(&self, width: u32, height: u32) -> (u32, u32) {
        let width = if (self.x + self.width) > width {
            width - self.x
        } else {
            self.width
        };
        let height = if (self.y + self.height) > height {
            height - self.y
        } else {
            self.height
        };
        (width, height)
    }
}

/// Wrapper over [`glyph_brush::GlyphBrush`]. Draws text.
///
/// Used for queuing and rendering text with [`TextBrush::draw`] and [`TextBrush::draw_custom`].
pub struct TextBrush<F = FontArc, H = DefaultSectionHasher> {
    inner: glyph_brush::GlyphBrush<Vertex, Extra, F, H>,
    pipeline: Pipeline,
}

impl<F, H> TextBrush<F, H>
where
    F: Font + Sync,
    H: std::hash::BuildHasher,
{
    /// Queues section for drawing. This should be called every frame for every section that is going to be drawn.
    ///
    /// This can be called multiple times for different sections that want to use the
    /// same font and gpu cache.
    #[inline]
    pub fn queue<'a, S>(&mut self, section: S)
    where
        S: Into<std::borrow::Cow<'a, Section<'a>>>,
    {
        self.inner.queue(section);
    }

    fn draw_queued(
        &mut self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        queue: &wgpu::Queue,
        config: &wgpu::SurfaceConfiguration,
        region: Option<ScissorRegion>,
    ) -> wgpu::CommandBuffer {
        let mut brush_action;

        loop {
            brush_action = self.inner.process_queued(
                |rect, data| self.pipeline.update_texture(rect, data, queue),
                Vertex::to_vertex,
            );

            match brush_action {
                Ok(_) => break,

                Err(glyph_brush::BrushError::TextureTooSmall { suggested }) => {
                    if log::log_enabled!(log::Level::Warn) {
                        log::warn!(
                            "Resizing cache texture! This should be avoided \
                            by building TextBrush with BrushBuilder::initial_cache_size() \
                            and providing cache texture dimensions."
                        );
                    }
                    let max_image_dimension = device.limits().max_texture_dimension_2d;
                    let (width, height) = if (suggested.0 > max_image_dimension
                        || suggested.1 > max_image_dimension)
                        && (self.inner.texture_dimensions().0 < max_image_dimension
                            || self.inner.texture_dimensions().1 < max_image_dimension)
                    {
                        (max_image_dimension, max_image_dimension)
                    } else {
                        suggested
                    };
                    self.pipeline.resize_texture(device, width, height);
                    self.inner.resize_texture(width, height);
                }
            }
        }

        match brush_action.unwrap() {
            BrushAction::Draw(vertices) => self.pipeline.update_buffer(vertices, device, queue),
            BrushAction::ReDraw => (),
        }

        self.pipeline.draw(device, view, config, region)
    }

    /// Draws all queued sections with [`queue`](#method.queue) function.
    ///
    /// Use [`TextBrush::draw_custom`] for more rendering options.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        queue: &wgpu::Queue,
        config: &wgpu::SurfaceConfiguration,
    ) -> wgpu::CommandBuffer {
        self.draw_queued(device, view, queue, config, None)
    }

    /// Draws all queued text with extra options.
    ///
    /// # Scissoring
    /// With scissoring, you can filter out each glyph fragment that crosses the given `region`.
    pub fn draw_custom<R>(
        &mut self,
        device: &wgpu::Device,
        view: &wgpu::TextureView,
        queue: &wgpu::Queue,
        config: &wgpu::SurfaceConfiguration,
        region: Option<R>,
    ) -> wgpu::CommandBuffer
    where
        R: Into<ScissorRegion>,
    {
        self.draw_queued(device, view, queue, config, region.map(|r| r.into()))
    }

    /// Resizes "_camera_". Updates default orthogonal view matrix
    /// with given arguments and uses it for rendering.
    ///
    /// Run this function whenever the surface is resized.
    /// _width_ and _height_ should be **surfaces** dimensions.
    #[inline]
    pub fn resize_view(&mut self, width: f32, height: f32, queue: &wgpu::Queue) {
        let matrix = ortho(width, height);
        self.pipeline.update_matrix(matrix, queue);
    }

    /// Provides your own matrix for rendering instead of default orthogonal view matrix.
    #[inline]
    pub fn custom_matrix<M>(&mut self, matrix: M, queue: &wgpu::Queue)
    where
        M: Into<[f32; 16]>,
    {
        self.pipeline.update_matrix(matrix.into(), queue);
    }
}

/// Builder for [`TextBrush`].
pub struct BrushBuilder<F, H = DefaultSectionHasher> {
    inner: glyph_brush::GlyphBrushBuilder<F, H>,
}

impl BrushBuilder<()> {
    /// Creates a [`BrushBuilder`] with [`Font`].
    #[inline]
    pub fn using_font<F: Font>(font: F) -> BrushBuilder<F> {
        BrushBuilder::using_fonts(vec![font])
    }

    /// Creates a [`BrushBuilder`] with font byte data.
    #[inline]
    pub fn using_font_bytes(data: &[u8]) -> Result<BrushBuilder<FontRef>, InvalidFont> {
        let font = FontRef::try_from_slice(data)?;
        Ok(BrushBuilder::using_fonts(vec![font]))
    }

    /// Creates a [`BrushBuilder`] with multiple fonts byte data.
    #[inline]
    pub fn using_font_bytes_vec(data: &[u8]) -> Result<BrushBuilder<FontRef>, InvalidFont> {
        let font = FontRef::try_from_slice(data)?;
        Ok(BrushBuilder::using_fonts(vec![font]))
    }

    /// Creates a [`BrushBuilder`] with multiple [`Font`].
    pub fn using_fonts<F: Font>(fonts: Vec<F>) -> BrushBuilder<F> {
        BrushBuilder {
            inner: glyph_brush::GlyphBrushBuilder::using_fonts(fonts),
        }
    }
}

impl<F, H> BrushBuilder<F, H>
where
    F: Font,
    H: std::hash::BuildHasher,
{
    glyph_brush::delegate_glyph_brush_builder_fns!(inner);

    /// Builds a [`TextBrush`] consuming [`BrushBuilder`].
    pub fn build(
        self,
        device: &wgpu::Device,
        render_format: wgpu::TextureFormat,
        width: f32,
        height: f32,
    ) -> TextBrush<F, H> {
        let inner = self.inner.build();
        let matrix = ortho(width, height);
        let pipeline = Pipeline::new(device, render_format, inner.texture_dimensions(), matrix);
        TextBrush { inner, pipeline }
    }
}

#[rustfmt::skip]
fn ortho(width: f32, height: f32) -> [f32; 16] {
    [
        2.0 / width, 0.0,          0.0, 0.0,
        0.0,        -2.0 / height, 0.0, 0.0,
        0.0,         0.0,          1.0, 0.0,
       -1.0,         1.0,          0.0, 1.0,
    ]
}
