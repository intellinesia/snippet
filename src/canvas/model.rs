//! Data model for the agent-first canvas.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::html::{DomOp, HtmlError};
use super::render::RenderError;

pub const CANVAS_FORMAT_VERSION: u32 = 1;
pub const DEFAULT_FRAME_W: f32 = 1440.0;
pub const DEFAULT_FRAME_H: f32 = 900.0;

pub fn default_format_version() -> u32 {
    CANVAS_FORMAT_VERSION
}

pub fn default_frame_w() -> f32 {
    DEFAULT_FRAME_W
}

pub fn default_frame_h() -> f32 {
    DEFAULT_FRAME_H
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub id: String,
    pub title: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub html: String,
    pub rev: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    User,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub kind: AuthorKind,
    pub id: String,
}

impl Author {
    pub fn user(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::User,
            id: id.into(),
        }
    }

    pub fn agent(id: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::Agent,
            id: id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    CreateFrame {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        title: String,
        #[serde(default)]
        x: f32,
        #[serde(default)]
        y: f32,
        #[serde(default = "default_frame_w")]
        w: f32,
        #[serde(default = "default_frame_h")]
        h: f32,
        #[serde(default)]
        html: String,
    },
    SetHtml {
        frame_id: String,
        html: String,
    },
    Dom {
        frame_id: String,
        #[serde(flatten)]
        dom: DomOp,
    },
    MoveFrame {
        frame_id: String,
        x: f32,
        y: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        h: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    DeleteFrame {
        frame_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    pub rev: u64,
    pub batch_id: String,
    pub author: Author,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub ts: i64,
    pub label: String,
    pub ops: Vec<Op>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inverses: Vec<Op>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverts: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinStatus {
    Open,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub id: String,
    pub frame_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub el_id: Option<String>,
    pub dx: f32,
    pub dy: f32,
    pub text: String,
    pub author: Author,
    pub status: PinStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    pub created_ts: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanvasSnapshot {
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    pub canvas_id: String,
    pub rev: u64,
    pub frames: Vec<Frame>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinsFile {
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    pub pins: Vec<Pin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchCommit {
    pub canvas_id: String,
    pub rev: u64,
    pub batch_id: String,
    pub label: String,
    pub frame_ids: Vec<String>,
    pub created_ids: Vec<String>,
    pub links: Vec<String>,
}

#[derive(Debug, Error)]
pub enum CanvasError {
    #[error(
        "invalid id `{0}` (use 1..=128 ASCII letters, digits, '-', '_', '.', '~'; no leading '.' or '..')"
    )]
    InvalidId(String),
    #[error("frame `{0}` not found (call canvas_read to list available frame ids)")]
    FrameNotFound(String),
    #[error("frame `{0}` already exists (use set_html, dom, or move_frame to update it)")]
    FrameExists(String),
    #[error("batch `{0}` not found")]
    BatchNotFound(String),
    #[error("pin `{0}` not found (call canvas_pins with action `list` to see pin ids)")]
    PinNotFound(String),
    #[error("batch must contain at least one op")]
    EmptyBatch,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("unsupported canvas persistence format version {found} (supported up to {supported})")]
    UnsupportedFormatVersion { found: u32, supported: u32 },
    #[error("html error: {0}")]
    Html(#[from] HtmlError),
    #[error("render error: {0}")]
    Render(#[from] RenderError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}
