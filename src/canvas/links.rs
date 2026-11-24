//! Canvas link grammar: `canvas:<canvas_id>/<frame_id>[/<el_id>][@<rev>]`
//! and markdown formatting `[title](canvas:...)`.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasLink {
    pub canvas_id: String,
    pub frame_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub el_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LinkError {
    #[error("canvas link must start with `canvas:`")]
    MissingScheme,
    #[error("canvas link must include `<canvas_id>/<frame_id>`")]
    MissingComponents,
    #[error("canvas link has too many path segments")]
    TooManySegments,
    #[error("invalid canvas link component: `{0}`")]
    InvalidComponent(String),
    #[error("invalid canvas revision: `{0}`")]
    InvalidRev(String),
}

pub const MAX_COMPONENT_LEN: usize = 128;

pub fn is_safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_COMPONENT_LEN
        && !s.starts_with('.')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

impl CanvasLink {
    pub fn new(
        canvas_id: impl Into<String>,
        frame_id: impl Into<String>,
        el_id: Option<String>,
        rev: Option<u64>,
    ) -> Self {
        Self {
            canvas_id: canvas_id.into(),
            frame_id: frame_id.into(),
            el_id,
            rev,
        }
    }

    pub fn parse(raw: &str) -> Result<Self, LinkError> {
        parse_canvas_ref(raw)
    }

    pub fn to_ref(&self) -> String {
        let mut out = format!("canvas:{}/{}", self.canvas_id, self.frame_id);
        if let Some(el_id) = &self.el_id {
            out.push('/');
            out.push_str(el_id);
        }
        if let Some(rev) = self.rev {
            out.push('@');
            out.push_str(&rev.to_string());
        }
        out
    }

    pub fn to_markdown(&self, title: &str) -> String {
        format_markdown_link(title, self)
    }
}

impl fmt::Display for CanvasLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_ref())
    }
}

pub fn parse_canvas_ref(raw: &str) -> Result<CanvasLink, LinkError> {
    let body = raw
        .trim()
        .strip_prefix("canvas:")
        .ok_or(LinkError::MissingScheme)?;

    let (path, rev) = match body.rsplit_once('@') {
        Some((p, r)) => {
            if r.is_empty() {
                return Err(LinkError::InvalidRev(r.to_string()));
            }
            let parsed = r
                .parse::<u64>()
                .map_err(|_| LinkError::InvalidRev(r.to_string()))?;
            (p, Some(parsed))
        }
        None => (body, None),
    };

    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [canvas_id, frame_id] => {
            if !is_safe_component(canvas_id) {
                return Err(LinkError::InvalidComponent((*canvas_id).to_string()));
            }
            if !is_safe_component(frame_id) {
                return Err(LinkError::InvalidComponent((*frame_id).to_string()));
            }
            Ok(CanvasLink {
                canvas_id: (*canvas_id).to_string(),
                frame_id: (*frame_id).to_string(),
                el_id: None,
                rev,
            })
        }
        [canvas_id, frame_id, el_id] => {
            if !is_safe_component(canvas_id) {
                return Err(LinkError::InvalidComponent((*canvas_id).to_string()));
            }
            if !is_safe_component(frame_id) {
                return Err(LinkError::InvalidComponent((*frame_id).to_string()));
            }
            if !is_safe_component(el_id) {
                return Err(LinkError::InvalidComponent((*el_id).to_string()));
            }
            Ok(CanvasLink {
                canvas_id: (*canvas_id).to_string(),
                frame_id: (*frame_id).to_string(),
                el_id: Some((*el_id).to_string()),
                rev,
            })
        }
        [_] | [] => Err(LinkError::MissingComponents),
        _ => Err(LinkError::TooManySegments),
    }
}

pub fn format_markdown_link(title: &str, link: &CanvasLink) -> String {
    let clean: String = title
        .chars()
        .map(|c| match c {
            '[' | ']' | '\n' | '\r' => ' ',
            c => c,
        })
        .collect();
    let label = clean.trim();
    let label = if label.is_empty() {
        link.frame_id.as_str()
    } else {
        label
    };
    format!("[{label}]({})", link.to_ref())
}

/// Extract all valid `[title](canvas:...)` links from markdown text without altering the text.
pub fn extract_markdown_links(markdown: &str) -> Vec<(String, CanvasLink)> {
    let mut out = Vec::new();
    let bytes = markdown.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'['
            && let Some(close_rel) = markdown[i + 1..].find(']')
        {
            let close = i + 1 + close_rel;
            if markdown.as_bytes().get(close + 1) == Some(&b'(')
                && let Some(end_rel) = markdown[close + 2..].find(')')
            {
                let end = close + 2 + end_rel;
                let target = &markdown[close + 2..end];
                if let Ok(link) = parse_canvas_ref(target) {
                    let title = markdown[i + 1..close].to_string();
                    out.push((title, link));
                    i = end + 1;
                    continue;
                }
            }
        }
        // Advance by one UTF-8 scalar value.
        let step = markdown[i..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        i += step;
    }
    out
}
