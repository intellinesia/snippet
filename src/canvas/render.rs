//! Frame renderer trait and stub implementation.
//! The Blitz renderer lives behind [`FrameRenderer`] and is wired separately.

use std::io::Cursor;

use image::{ImageFormat, Rgb, RgbImage};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_RENDER_EDGE: u32 = 8192;
pub const MAX_RENDER_PIXELS: u64 = 33_554_432;
pub const MIN_RENDER_SCALE: f32 = 0.25;
pub const MAX_RENDER_SCALE: f32 = 3.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementRect {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Rendered {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub layout: Vec<ElementRect>,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("layout error: {0}")]
    Layout(String),
    #[error("paint error: {0}")]
    Paint(String),
    #[error("render dimensions or HTML payload too large")]
    TooLarge,
    #[error("render timed out")]
    Timeout,
    #[error("render queue saturated; retry shortly")]
    Overloaded,
}

pub trait FrameRenderer: Send + Sync {
    fn render(
        &self,
        html: &str,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<Rendered, RenderError>;

    fn hit_test(&self, html: &str, width: u32, x: f32, y: f32) -> Option<String>;
}

/// Blitz-backed renderer (CPU, no browser, bundled fonts).
#[derive(Debug, Default, Clone, Copy)]
pub struct BlitzRenderer;

fn map_err(e: canvas_render::RenderError) -> RenderError {
    match e {
        canvas_render::RenderError::Parse(s) => RenderError::Parse(s),
        canvas_render::RenderError::Layout(s) => RenderError::Layout(s),
        canvas_render::RenderError::Paint(s) => RenderError::Paint(s),
        canvas_render::RenderError::TooLarge => RenderError::TooLarge,
    }
}

impl FrameRenderer for BlitzRenderer {
    fn render(
        &self,
        html: &str,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<Rendered, RenderError> {
        let r = canvas_render::render(html, width, height, scale).map_err(map_err)?;
        Ok(Rendered {
            png: r.png,
            width: r.width,
            height: r.height,
            layout: r
                .layout
                .into_iter()
                .map(|e| ElementRect {
                    id: e.id,
                    x: e.x,
                    y: e.y,
                    w: e.w,
                    h: e.h,
                    tag: e.tag,
                    text: e.text,
                })
                .collect(),
        })
    }

    fn hit_test(&self, html: &str, width: u32, x: f32, y: f32) -> Option<String> {
        canvas_render::hit_test(html, width, x, y)
    }
}

/// Placeholder renderer that produces a flat grey PNG of the scaled size with an empty layout.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubRenderer;

impl FrameRenderer for StubRenderer {
    fn render(
        &self,
        _html: &str,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<Rendered, RenderError> {
        if !scale.is_finite() || !(MIN_RENDER_SCALE..=MAX_RENDER_SCALE).contains(&scale) {
            return Err(RenderError::Layout(format!(
                "scale must be between {MIN_RENDER_SCALE} and {MAX_RENDER_SCALE}"
            )));
        }
        let pw = ((width.max(1) as f64) * (scale as f64)).round();
        let ph = ((height.max(1) as f64) * (scale as f64)).round();
        if pw < 1.0
            || ph < 1.0
            || pw > MAX_RENDER_EDGE as f64
            || ph > MAX_RENDER_EDGE as f64
            || (pw as u64).saturating_mul(ph as u64) > MAX_RENDER_PIXELS
        {
            return Err(RenderError::TooLarge);
        }
        let (pw, ph) = (pw as u32, ph as u32);
        let img = RgbImage::from_pixel(pw, ph, Rgb([224, 224, 224]));
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png)
            .map_err(|e| RenderError::Paint(e.to_string()))?;
        Ok(Rendered {
            png: buf.into_inner(),
            width: pw,
            height: ph,
            layout: Vec::new(),
        })
    }

    fn hit_test(&self, _html: &str, _width: u32, _x: f32, _y: f32) -> Option<String> {
        None
    }
}
