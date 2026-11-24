//! Persistent per-project canvas store, append-only op log, snapshots, pins, and render cache.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::html::{self, IdGen};
use super::links::{CanvasLink, format_markdown_link, is_safe_component};
use super::model::{
    Author, Batch, BatchCommit, CANVAS_FORMAT_VERSION, CanvasError, CanvasSnapshot, Frame, Op, Pin,
    PinStatus, PinsFile,
};
use super::render::{
    ElementRect, FrameRenderer, MAX_RENDER_SCALE, MIN_RENDER_SCALE, RenderError, Rendered,
    StubRenderer,
};

pub const SNAPSHOT_INTERVAL: u64 = 50;
pub const THUMB_LINK_TTL_SECS: i64 = 6 * 60 * 60;
pub const MAX_FRAME_W: f32 = 4000.0;
pub const MAX_FRAME_H: f32 = 8000.0;
pub const MAX_BATCH_OPS: usize = 200;
pub const MAX_FRAMES_PER_CANVAS: usize = 500;
pub const MAX_PIN_TEXT_CHARS: usize = 4_000;
pub const MAX_LABEL_CHARS: usize = 256;
pub const MAX_CACHE_ENTRIES: usize = 64;
pub const MAX_STORES_CACHED: usize = 64;
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(15);
pub const HIT_TEST_TIMEOUT: Duration = Duration::from_secs(5);

const MAX_CONCURRENT_RENDERS: usize = 4;
const MIN_SCALE_MILLIS: u32 = 250;
const MAX_SCALE_MILLIS: u32 = 3000;

const OPS_FILE: &str = "ops.jsonl";
const META_FILE: &str = "meta.json";
const MAX_CANVASES_LISTED: usize = 200;
const MAX_CANVAS_TITLE_CHARS: usize = 120;
const PINS_FILE: &str = "pins.json";
const SNAPSHOTS_DIR: &str = "snapshots";
const CACHE_DIR: &str = "cache";

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum PinsOnDisk {
    Versioned(PinsFile),
    Legacy(Vec<Pin>),
}

fn check_format_version(found: u32) -> Result<(), CanvasError> {
    if found == 0 || found > CANVAS_FORMAT_VERSION {
        return Err(CanvasError::UnsupportedFormatVersion {
            found,
            supported: CANVAS_FORMAT_VERSION,
        });
    }
    Ok(())
}

pub fn canvases_root() -> PathBuf {
    crate::config::snippet_home().join("canvases")
}

/// Derive a stable per-project canvas id from a workspace path (shared across git worktrees).
pub fn canvas_id_for_workspace(workspace: &Path) -> String {
    let repo = crate::memory::MemoryRepo::project(workspace);
    let raw = repo.key.strip_prefix("project:").unwrap_or(&repo.key);
    if is_safe_component(raw) {
        raw.to_string()
    } else {
        "default".to_string()
    }
}

pub fn normalize_scale(scale: f32) -> Result<(f32, u32), CanvasError> {
    if !scale.is_finite() || !(MIN_RENDER_SCALE..=MAX_RENDER_SCALE).contains(&scale) {
        return Err(CanvasError::InvalidArgument(format!(
            "scale must be between {MIN_RENDER_SCALE} and {MAX_RENDER_SCALE}"
        )));
    }
    let millis = (scale * 1000.0).round() as u32;
    let millis = millis.clamp(MIN_SCALE_MILLIS, MAX_SCALE_MILLIS);
    Ok((millis as f32 / 1000.0, millis))
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 32 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "frame".to_string()
    } else {
        out
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CanvasError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let write_res = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_data()?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(key_block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

fn thumb_signature(secret: &str, scope: &str, exp: i64) -> String {
    hmac_sha256(
        secret.as_bytes(),
        format!("canvas-thumb:{scope}:{exp}").as_bytes(),
    )
    .iter()
    .take(16)
    .map(|b| format!("{b:02x}"))
    .collect()
}

/// Build a signed thumbnail URL under `/a/canvas/...` so chat cards can load it without the daemon token.
pub fn signed_thumb_path(
    secret: &str,
    canvas_id: &str,
    frame_id: &str,
    rev: u64,
    scale_millis: u32,
    ttl_secs: i64,
) -> String {
    let exp = chrono::Utc::now().timestamp() + ttl_secs;
    let scope = format!("{canvas_id}/{frame_id}/{rev}/{scale_millis}");
    let sig = thumb_signature(secret, &scope, exp);
    format!("/a/canvas/{exp}/{sig}/{scope}/thumb.png")
}

pub fn verify_signed_thumb(
    secret: &str,
    exp: i64,
    sig: &str,
    canvas_id: &str,
    frame_id: &str,
    rev: u64,
    scale_millis: u32,
) -> bool {
    if exp < chrono::Utc::now().timestamp() {
        return false;
    }
    if !is_safe_component(canvas_id)
        || !is_safe_component(frame_id)
        || !(MIN_SCALE_MILLIS..=MAX_SCALE_MILLIS).contains(&scale_millis)
    {
        return false;
    }
    let scope = format!("{canvas_id}/{frame_id}/{rev}/{scale_millis}");
    let expected = thumb_signature(secret, &scope, exp);
    expected.len() == sig.len()
        && expected
            .bytes()
            .zip(sig.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[derive(Debug)]
pub struct CanvasStore {
    root: PathBuf,
    canvas_id: String,
    rev: u64,
    frames: BTreeMap<String, Frame>,
    ops_log: Vec<Batch>,
    pins: Vec<Pin>,
    meta: CanvasMeta,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CanvasMeta {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub created_ts: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CanvasSummary {
    pub canvas_id: String,
    pub title: String,
    pub frames: usize,
    pub rev: u64,
    pub updated_at: i64,
    pub author: Option<String>,
    #[serde(skip)]
    pub thumb_frame: Option<(String, u64)>,
}

impl CanvasStore {
    pub fn open(
        base_root: impl Into<PathBuf>,
        canvas_id: impl Into<String>,
    ) -> Result<Self, CanvasError> {
        let canvas_id = canvas_id.into();
        if !is_safe_component(&canvas_id) {
            return Err(CanvasError::InvalidId(canvas_id));
        }
        let root = base_root.into().join(&canvas_id);
        let mut store = Self {
            root,
            canvas_id,
            rev: 0,
            frames: BTreeMap::new(),
            ops_log: Vec::new(),
            pins: Vec::new(),
            meta: CanvasMeta::default(),
        };
        if store.root.is_dir() {
            store.load_from_disk()?;
            if let Ok(bytes) = fs::read(store.root.join(META_FILE))
                && let Ok(meta) = serde_json::from_slice(&bytes)
            {
                store.meta = meta;
            }
        }
        Ok(store)
    }

    pub fn canvas_id(&self) -> &str {
        &self.canvas_id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn title(&self) -> String {
        self.meta
            .title
            .clone()
            .unwrap_or_else(|| self.canvas_id.clone())
    }

    pub fn summary(&self) -> CanvasSummary {
        CanvasSummary {
            canvas_id: self.canvas_id.clone(),
            title: self.title(),
            frames: self.frames.len(),
            rev: self.rev,
            updated_at: self
                .ops_log
                .last()
                .map(|b| b.ts)
                .unwrap_or(self.meta.created_ts),
            author: self.ops_log.last().map(|b| b.author.id.clone()),
            thumb_frame: self.frames.values().next().map(|f| (f.id.clone(), f.rev)),
        }
    }

    fn write_meta(&self) -> Result<(), CanvasError> {
        fs::create_dir_all(&self.root)?;
        fs::write(self.root.join(META_FILE), serde_json::to_vec(&self.meta)?)?;
        Ok(())
    }

    pub fn frames(&self) -> Vec<Frame> {
        self.frames.values().cloned().collect()
    }

    pub fn frame(&self, frame_id: &str) -> Option<Frame> {
        self.frames.get(frame_id).cloned()
    }

    pub fn ops_since(&self, since_rev: u64) -> Vec<Batch> {
        self.ops_log
            .iter()
            .filter(|b| b.rev > since_rev)
            .cloned()
            .collect()
    }

    pub fn batch(&self, batch_id: &str) -> Option<Batch> {
        self.ops_log
            .iter()
            .find(|b| b.batch_id == batch_id)
            .cloned()
    }

    pub fn pins(&self, frame_id: Option<&str>, status: Option<PinStatus>) -> Vec<Pin> {
        self.pins
            .iter()
            .filter(|p| frame_id.is_none_or(|fid| p.frame_id == fid))
            .filter(|p| status.is_none_or(|st| p.status == st))
            .cloned()
            .collect()
    }

    pub fn add_pin(
        &mut self,
        frame_id: String,
        el_id: Option<String>,
        dx: f32,
        dy: f32,
        text: String,
        author: Author,
    ) -> Result<Pin, CanvasError> {
        if !is_safe_component(&frame_id) {
            return Err(CanvasError::InvalidId(frame_id));
        }
        if let Some(ref el) = el_id
            && !is_safe_component(el)
        {
            return Err(CanvasError::InvalidId(el.clone()));
        }
        if !self.frames.contains_key(&frame_id) {
            return Err(CanvasError::FrameNotFound(frame_id));
        }
        if !dx.is_finite()
            || !dy.is_finite()
            || !(0.0..=1.0).contains(&dx)
            || !(0.0..=1.0).contains(&dy)
        {
            return Err(CanvasError::InvalidArgument(
                "dx and dy must be finite fractions in 0.0..=1.0".into(),
            ));
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(CanvasError::InvalidArgument(
                "pin text cannot be empty".into(),
            ));
        }
        if text.chars().count() > MAX_PIN_TEXT_CHARS {
            return Err(CanvasError::InvalidArgument(format!(
                "pin text exceeds {MAX_PIN_TEXT_CHARS} characters"
            )));
        }
        let pin = Pin {
            id: format!("pin-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            frame_id,
            el_id,
            dx,
            dy,
            text,
            author,
            status: PinStatus::Open,
            reply: None,
            created_ts: chrono::Utc::now().timestamp(),
        };
        let mut next_pins = self.pins.clone();
        next_pins.push(pin.clone());
        self.write_pins_slice(&next_pins)?;
        self.pins = next_pins;
        Ok(pin)
    }

    pub fn update_pin(
        &mut self,
        pin_id: &str,
        status: Option<PinStatus>,
        reply: Option<String>,
        text: Option<String>,
    ) -> Result<Pin, CanvasError> {
        if !is_safe_component(pin_id) {
            return Err(CanvasError::InvalidId(pin_id.to_string()));
        }
        if let Some(ref r) = reply
            && r.chars().count() > MAX_PIN_TEXT_CHARS
        {
            return Err(CanvasError::InvalidArgument(format!(
                "pin reply exceeds {MAX_PIN_TEXT_CHARS} characters"
            )));
        }
        if let Some(ref t) = text
            && t.trim().chars().count() > MAX_PIN_TEXT_CHARS
        {
            return Err(CanvasError::InvalidArgument(format!(
                "pin text exceeds {MAX_PIN_TEXT_CHARS} characters"
            )));
        }
        let mut next_pins = self.pins.clone();
        let Some(pin) = next_pins.iter_mut().find(|p| p.id == pin_id) else {
            return Err(CanvasError::PinNotFound(pin_id.to_string()));
        };
        if let Some(status) = status {
            pin.status = status;
        }
        if let Some(reply) = reply {
            pin.reply = Some(reply);
        }
        if let Some(text) = text {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                pin.text = trimmed.to_string();
            }
        }
        let updated = pin.clone();
        self.write_pins_slice(&next_pins)?;
        self.pins = next_pins;
        Ok(updated)
    }

    /// Atomically apply a batch of ops: if any op fails, nothing is mutated or persisted.
    pub fn apply_batch(
        &mut self,
        label: String,
        ops: Vec<Op>,
        author: Author,
        session: Option<String>,
    ) -> Result<BatchCommit, CanvasError> {
        self.commit_ops(label, ops, author, session, None)
    }

    /// Revert a previous batch by appending an inverse batch computed from its recorded inverses.
    /// History is never rewritten, so revert-of-revert works naturally.
    pub fn revert_batch(
        &mut self,
        target_batch_id: &str,
        author: Author,
        session: Option<String>,
    ) -> Result<BatchCommit, CanvasError> {
        let target = self
            .ops_log
            .iter()
            .find(|b| b.batch_id == target_batch_id)
            .cloned()
            .ok_or_else(|| CanvasError::BatchNotFound(target_batch_id.to_string()))?;
        if target.inverses.is_empty() {
            return Err(CanvasError::InvalidArgument(format!(
                "batch {target_batch_id} has no inverse ops"
            )));
        }
        let label = format!("Revert: {}", target.label);
        self.commit_ops(
            label,
            target.inverses,
            author,
            session,
            Some(target.batch_id),
        )
    }

    fn commit_ops(
        &mut self,
        label: String,
        ops: Vec<Op>,
        author: Author,
        session: Option<String>,
        reverts: Option<String>,
    ) -> Result<BatchCommit, CanvasError> {
        if ops.is_empty() {
            return Err(CanvasError::EmptyBatch);
        }
        if ops.len() > MAX_BATCH_OPS {
            return Err(CanvasError::InvalidArgument(format!(
                "batch has {} ops; maximum is {MAX_BATCH_OPS}",
                ops.len()
            )));
        }
        if label.chars().count() > MAX_LABEL_CHARS {
            return Err(CanvasError::InvalidArgument(format!(
                "batch label exceeds {MAX_LABEL_CHARS} characters"
            )));
        }
        let next_rev = self.rev + 1;
        let mut staging = self.frames.clone();
        let mut id_gen = Self::seed_id_gen(&staging);

        let mut concrete_ops = Vec::with_capacity(ops.len());
        let mut inverses = Vec::with_capacity(ops.len());
        let mut touched_frames: Vec<String> = Vec::new();
        let mut created_ids: Vec<String> = Vec::new();
        let mut seen_touched: HashSet<String> = HashSet::new();

        for op in ops {
            html::clear_dropped();
            let applied = Self::apply_single_op(&mut staging, op, next_rev, &mut id_gen);
            let dropped = html::take_dropped();
            if !dropped.is_empty() {
                return Err(CanvasError::Html(html::HtmlError::Unsupported(dropped)));
            }
            let (concrete, inverse, frame_id, new_ids) = applied?;
            if seen_touched.insert(frame_id.clone()) {
                touched_frames.push(frame_id);
            }
            created_ids.extend(new_ids);
            concrete_ops.push(concrete);
            inverses.push(inverse);
        }

        // Inverses must run in reverse order to undo the batch.
        inverses.reverse();

        let batch = Batch {
            format_version: CANVAS_FORMAT_VERSION,
            rev: next_rev,
            batch_id: format!("b-{}", &uuid::Uuid::new_v4().to_string()[..12]),
            author,
            session,
            ts: chrono::Utc::now().timestamp(),
            label: label.clone(),
            ops: concrete_ops,
            inverses,
            reverts,
        };

        self.append_batch_to_log(&batch)?;
        self.rev = next_rev;
        self.frames = staging;
        self.ops_log.push(batch.clone());

        if self.rev.is_multiple_of(SNAPSHOT_INTERVAL) {
            // Snapshot is a replay optimization; ops.jsonl is authoritative so a snapshot failure does not uncommit the batch.
            let _ = self.write_snapshot();
        }

        let links: Vec<String> = touched_frames
            .iter()
            .map(|fid| {
                let title = self
                    .frames
                    .get(fid)
                    .map(|f| f.title.as_str())
                    .unwrap_or(fid.as_str());
                let link = CanvasLink::new(&self.canvas_id, fid, None, Some(self.rev));
                format_markdown_link(title, &link)
            })
            .collect();

        Ok(BatchCommit {
            canvas_id: self.canvas_id.clone(),
            rev: self.rev,
            batch_id: batch.batch_id,
            label,
            frame_ids: touched_frames,
            created_ids,
            links,
        })
    }

    fn seed_id_gen(frames: &BTreeMap<String, Frame>) -> IdGen {
        let mut existing = Vec::new();
        for frame in frames.values() {
            existing.extend(html::element_ids(&frame.html));
        }
        IdGen::new(&existing)
    }

    fn apply_single_op(
        frames: &mut BTreeMap<String, Frame>,
        op: Op,
        rev: u64,
        ids: &mut IdGen,
    ) -> Result<(Op, Op, String, Vec<String>), CanvasError> {
        match op {
            Op::CreateFrame {
                id,
                title,
                x,
                y,
                w,
                h,
                html: raw_html,
            } => {
                if !x.is_finite()
                    || !y.is_finite()
                    || !w.is_finite()
                    || !h.is_finite()
                    || !(1.0..=MAX_FRAME_W).contains(&w)
                    || !(1.0..=MAX_FRAME_H).contains(&h)
                {
                    return Err(CanvasError::InvalidArgument(format!(
                        "frame geometry must be finite with w in 1..={MAX_FRAME_W} and h in 1..={MAX_FRAME_H}"
                    )));
                }
                if title.chars().count() > MAX_LABEL_CHARS {
                    return Err(CanvasError::InvalidArgument(format!(
                        "frame title exceeds {MAX_LABEL_CHARS} characters"
                    )));
                }
                if frames.len() >= MAX_FRAMES_PER_CANVAS {
                    return Err(CanvasError::InvalidArgument(format!(
                        "canvas frame limit ({MAX_FRAMES_PER_CANVAS}) reached"
                    )));
                }
                let frame_id = match id {
                    Some(explicit) => {
                        if !is_safe_component(&explicit) {
                            return Err(CanvasError::InvalidId(explicit));
                        }
                        if frames.contains_key(&explicit) {
                            return Err(CanvasError::FrameExists(explicit));
                        }
                        explicit
                    }
                    None => {
                        let base = slug(&title);
                        let mut candidate = base.clone();
                        let mut n = 2usize;
                        while frames.contains_key(&candidate) {
                            candidate = format!("{base}-{n}");
                            n += 1;
                        }
                        candidate
                    }
                };
                let normalized = html::normalize(&raw_html, None, ids)?;
                let new_el_ids = html::element_ids(&normalized.html);
                let frame = Frame {
                    id: frame_id.clone(),
                    title: title.clone(),
                    x,
                    y,
                    w,
                    h,
                    html: normalized.html.clone(),
                    rev,
                };
                frames.insert(frame_id.clone(), frame);
                let concrete = Op::CreateFrame {
                    id: Some(frame_id.clone()),
                    title,
                    x,
                    y,
                    w,
                    h,
                    html: normalized.html,
                };
                let inverse = Op::DeleteFrame {
                    frame_id: frame_id.clone(),
                };
                let mut created = vec![frame_id.clone()];
                created.extend(new_el_ids);
                Ok((concrete, inverse, frame_id, created))
            }
            Op::SetHtml {
                frame_id,
                html: raw_html,
            } => {
                let frame = frames
                    .get_mut(&frame_id)
                    .ok_or_else(|| CanvasError::FrameNotFound(frame_id.clone()))?;
                let old_html = frame.html.clone();
                let before: HashSet<String> = html::element_ids(&old_html).into_iter().collect();
                let normalized = html::normalize(&raw_html, Some(&old_html), ids)?;
                let new_ids: Vec<String> = html::element_ids(&normalized.html)
                    .into_iter()
                    .filter(|id| !before.contains(id))
                    .collect();
                frame.html = normalized.html.clone();
                frame.rev = rev;
                let concrete = Op::SetHtml {
                    frame_id: frame_id.clone(),
                    html: normalized.html,
                };
                let inverse = Op::SetHtml {
                    frame_id: frame_id.clone(),
                    html: old_html,
                };
                Ok((concrete, inverse, frame_id, new_ids))
            }
            Op::Dom { frame_id, dom } => {
                let frame = frames
                    .get_mut(&frame_id)
                    .ok_or_else(|| CanvasError::FrameNotFound(frame_id.clone()))?;
                let before: HashSet<String> = html::element_ids(&frame.html).into_iter().collect();
                let (next_html, concrete_dom, inv_dom) =
                    html::apply_dom_op_with_concrete(&frame.html, &dom, ids)?;
                let new_ids: Vec<String> = html::element_ids(&next_html)
                    .into_iter()
                    .filter(|id| !before.contains(id))
                    .collect();
                frame.html = next_html;
                frame.rev = rev;
                let concrete = Op::Dom {
                    frame_id: frame_id.clone(),
                    dom: concrete_dom,
                };
                let inverse = Op::Dom {
                    frame_id: frame_id.clone(),
                    dom: inv_dom,
                };
                Ok((concrete, inverse, frame_id, new_ids))
            }
            Op::MoveFrame {
                frame_id,
                x,
                y,
                w,
                h,
                title,
            } => {
                if !x.is_finite()
                    || !y.is_finite()
                    || w.is_some_and(|v| !v.is_finite() || !(1.0..=MAX_FRAME_W).contains(&v))
                    || h.is_some_and(|v| !v.is_finite() || !(1.0..=MAX_FRAME_H).contains(&v))
                {
                    return Err(CanvasError::InvalidArgument(format!(
                        "frame geometry must be finite with w in 1..={MAX_FRAME_W} and h in 1..={MAX_FRAME_H}"
                    )));
                }
                if title
                    .as_ref()
                    .is_some_and(|t| t.chars().count() > MAX_LABEL_CHARS)
                {
                    return Err(CanvasError::InvalidArgument(format!(
                        "frame title exceeds {MAX_LABEL_CHARS} characters"
                    )));
                }
                let frame = frames
                    .get_mut(&frame_id)
                    .ok_or_else(|| CanvasError::FrameNotFound(frame_id.clone()))?;
                let inverse = Op::MoveFrame {
                    frame_id: frame_id.clone(),
                    x: frame.x,
                    y: frame.y,
                    w: Some(frame.w),
                    h: Some(frame.h),
                    title: Some(frame.title.clone()),
                };
                frame.x = x;
                frame.y = y;
                if let Some(w) = w {
                    frame.w = w;
                }
                if let Some(h) = h {
                    frame.h = h;
                }
                if let Some(ref title) = title {
                    frame.title = title.clone();
                }
                frame.rev = rev;
                let concrete = Op::MoveFrame {
                    frame_id: frame_id.clone(),
                    x,
                    y,
                    w,
                    h,
                    title,
                };
                Ok((concrete, inverse, frame_id, Vec::new()))
            }
            Op::DeleteFrame { frame_id } => {
                let old = frames
                    .remove(&frame_id)
                    .ok_or_else(|| CanvasError::FrameNotFound(frame_id.clone()))?;
                let concrete = Op::DeleteFrame {
                    frame_id: frame_id.clone(),
                };
                let inverse = Op::CreateFrame {
                    id: Some(old.id),
                    title: old.title,
                    x: old.x,
                    y: old.y,
                    w: old.w,
                    h: old.h,
                    html: old.html,
                };
                Ok((concrete, inverse, frame_id, Vec::new()))
            }
        }
    }

    fn replay_op_into(
        frames: &mut BTreeMap<String, Frame>,
        op: &Op,
        rev: u64,
        ids: &mut IdGen,
    ) -> Result<(), CanvasError> {
        match op {
            Op::CreateFrame {
                id,
                title,
                x,
                y,
                w,
                h,
                html,
            } => {
                let fid = id.clone().ok_or_else(|| {
                    CanvasError::InvalidArgument("replayed CreateFrame missing id".into())
                })?;
                frames.insert(
                    fid.clone(),
                    Frame {
                        id: fid,
                        title: title.clone(),
                        x: *x,
                        y: *y,
                        w: *w,
                        h: *h,
                        html: html.clone(),
                        rev,
                    },
                );
            }
            Op::SetHtml { frame_id, html } => {
                let frame = frames
                    .get_mut(frame_id)
                    .ok_or_else(|| CanvasError::FrameNotFound(frame_id.clone()))?;
                frame.html = html.clone();
                frame.rev = rev;
            }
            Op::Dom { frame_id, dom } => {
                let frame = frames
                    .get_mut(frame_id)
                    .ok_or_else(|| CanvasError::FrameNotFound(frame_id.clone()))?;
                let (next_html, _) = html::apply_dom_op(&frame.html, dom, ids)?;
                frame.html = next_html;
                frame.rev = rev;
            }
            Op::MoveFrame {
                frame_id,
                x,
                y,
                w,
                h,
                title,
            } => {
                let frame = frames
                    .get_mut(frame_id)
                    .ok_or_else(|| CanvasError::FrameNotFound(frame_id.clone()))?;
                frame.x = *x;
                frame.y = *y;
                if let Some(w) = w {
                    frame.w = *w;
                }
                if let Some(h) = h {
                    frame.h = *h;
                }
                if let Some(title) = title {
                    frame.title = title.clone();
                }
                frame.rev = rev;
            }
            Op::DeleteFrame { frame_id } => {
                frames.remove(frame_id);
            }
        }
        Ok(())
    }

    fn append_batch_to_log(&self, batch: &Batch) -> Result<(), CanvasError> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join(OPS_FILE);
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let line = serde_json::to_string(batch)?;
        writeln!(file, "{line}")?;
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    fn write_snapshot(&self) -> Result<(), CanvasError> {
        let dir = self.root.join(SNAPSHOTS_DIR);
        fs::create_dir_all(&dir)?;
        let snap = CanvasSnapshot {
            format_version: CANVAS_FORMAT_VERSION,
            canvas_id: self.canvas_id.clone(),
            rev: self.rev,
            frames: self.frames.values().cloned().collect(),
        };
        let path = dir.join(format!("rev-{:08}.json", self.rev));
        let bytes = serde_json::to_vec_pretty(&snap)?;
        atomic_write(&path, &bytes)
    }

    fn write_pins_slice(&self, pins: &[Pin]) -> Result<(), CanvasError> {
        let doc = PinsFile {
            format_version: CANVAS_FORMAT_VERSION,
            pins: pins.to_vec(),
        };
        let bytes = serde_json::to_vec_pretty(&doc)?;
        atomic_write(&self.root.join(PINS_FILE), &bytes)
    }

    fn load_latest_snapshot(&self) -> Result<Option<CanvasSnapshot>, CanvasError> {
        let dir = self.root.join(SNAPSHOTS_DIR);
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(None);
        };
        let mut best: Option<(u64, PathBuf)> = None;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num) = name
                .strip_prefix("rev-")
                .and_then(|s| s.strip_suffix(".json"))
                .and_then(|s| s.parse::<u64>().ok())
                && best.as_ref().is_none_or(|(b, _)| num > *b)
            {
                best = Some((num, entry.path()));
            }
        }
        let Some((_, path)) = best else {
            return Ok(None);
        };
        let bytes = fs::read(path)?;
        let snap: CanvasSnapshot = serde_json::from_slice(&bytes)?;
        check_format_version(snap.format_version)?;
        Ok(Some(snap))
    }

    fn load_from_disk(&mut self) -> Result<(), CanvasError> {
        let ops_path = self.root.join(OPS_FILE);
        if ops_path.is_file() {
            let raw = fs::read_to_string(&ops_path)?;
            for line in raw.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let batch: Batch = serde_json::from_str(line)?;
                check_format_version(batch.format_version)?;
                self.ops_log.push(batch);
            }
        }

        let mut base_rev = 0u64;
        if let Some(snap) = self.load_latest_snapshot()? {
            base_rev = snap.rev;
            self.rev = snap.rev;
            for frame in snap.frames {
                self.frames.insert(frame.id.clone(), frame);
            }
        }

        for batch in &self.ops_log {
            if batch.rev <= base_rev {
                continue;
            }
            // Match commit_ops' per-batch ID seeding so legacy and concrete DomOps replay identically.
            let mut id_gen = Self::seed_id_gen(&self.frames);
            for op in &batch.ops {
                Self::replay_op_into(&mut self.frames, op, batch.rev, &mut id_gen)?;
            }
            self.rev = batch.rev;
        }

        let pins_path = self.root.join(PINS_FILE);
        if pins_path.is_file() {
            let bytes = fs::read(&pins_path)?;
            self.pins = match serde_json::from_slice::<PinsOnDisk>(&bytes)? {
                PinsOnDisk::Versioned(f) => {
                    check_format_version(f.format_version)?;
                    f.pins
                }
                PinsOnDisk::Legacy(pins) => pins,
            };
        }
        Ok(())
    }

    /// Reconstruct the frame state at a historical revision by replaying the op log from rev 0.
    pub fn frame_at_rev(
        &self,
        frame_id: &str,
        target_rev: u64,
    ) -> Result<Option<Frame>, CanvasError> {
        if !is_safe_component(frame_id) {
            return Err(CanvasError::InvalidId(frame_id.to_string()));
        }
        if target_rev >= self.rev {
            return Ok(self.frames.get(frame_id).cloned());
        }
        let mut frames = BTreeMap::new();
        for batch in &self.ops_log {
            if batch.rev > target_rev {
                break;
            }
            let mut ids = Self::seed_id_gen(&frames);
            for op in &batch.ops {
                Self::replay_op_into(&mut frames, op, batch.rev, &mut ids)?;
            }
        }
        Ok(frames.remove(frame_id))
    }

    pub fn cache_png_path(&self, frame_id: &str, rev: u64, scale_millis: u32) -> PathBuf {
        self.root
            .join(CACHE_DIR)
            .join(format!("{frame_id}-r{rev}-s{scale_millis}.png"))
    }

    pub fn cache_layout_path(&self, frame_id: &str, rev: u64, scale_millis: u32) -> PathBuf {
        self.root
            .join(CACHE_DIR)
            .join(format!("{frame_id}-r{rev}-s{scale_millis}.layout.json"))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedLayout {
    pub canvas_id: String,
    pub frame_id: String,
    pub rev: u64,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub layout: Vec<ElementRect>,
}

fn prune_cache_dir(cache_dir: &Path) {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut pngs: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("png") {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            pngs.push((mtime, path));
        }
    }
    if pngs.len() <= MAX_CACHE_ENTRIES {
        return;
    }
    pngs.sort_by_key(|(mtime, _)| *mtime);
    let to_remove = pngs.len() - MAX_CACHE_ENTRIES;
    for (_, png_path) in pngs.into_iter().take(to_remove) {
        let _ = fs::remove_file(&png_path);
        let layout_path = png_path.with_extension("layout.json");
        let _ = fs::remove_file(layout_path);
    }
}

fn sort_summaries(items: &mut [CanvasSummary]) {
    items.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.canvas_id.cmp(&b.canvas_id))
    });
}

/// Thread-safe multi-canvas service holding stores + the pluggable [`FrameRenderer`].
pub struct CanvasService {
    root: PathBuf,
    renderer: Arc<dyn FrameRenderer>,
    stores: Mutex<HashMap<String, Arc<Mutex<CanvasStore>>>>,
    render_semaphore: Arc<Semaphore>,
}

impl CanvasService {
    pub fn new(root: PathBuf, renderer: Arc<dyn FrameRenderer>) -> Self {
        Self {
            root,
            renderer,
            stores: Mutex::new(HashMap::new()),
            render_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_RENDERS)),
        }
    }

    pub fn with_stub_renderer(root: PathBuf) -> Self {
        Self::new(root, Arc::new(StubRenderer))
    }

    pub fn default_instance() -> Arc<Self> {
        static DEFAULT: OnceLock<Arc<CanvasService>> = OnceLock::new();
        DEFAULT
            .get_or_init(|| {
                Arc::new(Self::new(
                    canvases_root(),
                    Arc::new(super::render::BlitzRenderer),
                ))
            })
            .clone()
    }

    /// Create an empty titled canvas with an artifact-style id: slug of the title plus a short random suffix.
    pub fn create_canvas(&self, title: &str) -> Result<CanvasSummary, CanvasError> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > MAX_CANVAS_TITLE_CHARS {
            return Err(CanvasError::InvalidArgument(format!(
                "canvas title must be 1..={MAX_CANVAS_TITLE_CHARS} characters"
            )));
        }
        let base = slug(title);
        let base = if base == "frame" && !title.to_ascii_lowercase().starts_with("frame") {
            "canvas".to_string()
        } else {
            base
        };
        for _ in 0..8 {
            let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
            let id = format!("{base}-{suffix}");
            if self.root.join(&id).exists() {
                continue;
            }
            let store = self.store(&id)?;
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            s.meta = CanvasMeta {
                title: Some(title.to_string()),
                created_ts: chrono::Utc::now().timestamp(),
            };
            s.write_meta()?;
            return Ok(s.summary());
        }
        Err(CanvasError::InvalidArgument(
            "could not allocate a unique canvas id; retry".into(),
        ))
    }

    /// All canvases on disk, most recently updated first.
    pub fn list_canvases(&self) -> Result<Vec<CanvasSummary>, CanvasError> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten().take(MAX_CANVASES_LISTED * 4) {
            if !entry.path().is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !is_safe_component(&id) {
                continue;
            }
            let Ok(store) = self.store(&id) else { continue };
            let s = store.lock().unwrap_or_else(|e| e.into_inner());
            out.push(s.summary());
        }
        sort_summaries(&mut out);
        out.truncate(MAX_CANVASES_LISTED);
        Ok(out)
    }

    pub fn renderer(&self) -> &Arc<dyn FrameRenderer> {
        &self.renderer
    }

    pub fn store(&self, canvas_id: &str) -> Result<Arc<Mutex<CanvasStore>>, CanvasError> {
        if !is_safe_component(canvas_id) {
            return Err(CanvasError::InvalidId(canvas_id.to_string()));
        }
        let mut map = self.stores.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get(canvas_id) {
            return Ok(existing.clone());
        }
        if map.len() >= MAX_STORES_CACHED {
            // Evict idle unreferenced stores so typos or many canvases never grow memory unboundedly.
            map.retain(|_, arc| Arc::strong_count(arc) > 1);
        }
        let opened = Arc::new(Mutex::new(CanvasStore::open(&self.root, canvas_id)?));
        map.insert(canvas_id.to_string(), opened.clone());
        Ok(opened)
    }

    /// Render (or fetch from disk cache) a frame's PNG thumbnail and layout map, keyed by (frame, rev, scale).
    pub fn render_frame_cached(
        &self,
        canvas_id: &str,
        frame_id: &str,
        requested_rev: Option<u64>,
        scale: f32,
    ) -> Result<(PathBuf, Vec<u8>, CachedLayout), CanvasError> {
        if !is_safe_component(frame_id) {
            return Err(CanvasError::InvalidId(frame_id.to_string()));
        }
        let (scale, scale_millis) = normalize_scale(scale)?;
        let store_arc = self.store(canvas_id)?;
        let (frame, png_path, layout_path, cache_dir) = {
            let store = store_arc.lock().unwrap_or_else(|e| e.into_inner());
            let frame = match requested_rev {
                Some(rev) => store.frame_at_rev(frame_id, rev)?,
                None => store.frame(frame_id),
            }
            .ok_or_else(|| CanvasError::FrameNotFound(frame_id.to_string()))?;
            let cache_rev = frame.rev;
            let png_path = store.cache_png_path(&frame.id, cache_rev, scale_millis);
            let layout_path = store.cache_layout_path(&frame.id, cache_rev, scale_millis);
            let cache_dir = store.root().join(CACHE_DIR);
            (frame, png_path, layout_path, cache_dir)
        };

        if png_path.is_file()
            && layout_path.is_file()
            && let Ok(png) = fs::read(&png_path)
            && let Ok(layout_bytes) = fs::read(&layout_path)
            && let Ok(cached_layout) = serde_json::from_slice::<CachedLayout>(&layout_bytes)
        {
            return Ok((png_path, png, cached_layout));
        }

        let width = frame.w.round().max(1.0) as u32;
        let height = frame.h.round().max(1.0) as u32;
        let rendered: Rendered = self.renderer.render(&frame.html, width, height, scale)?;
        let cached_layout = CachedLayout {
            canvas_id: canvas_id.to_string(),
            frame_id: frame.id.clone(),
            rev: frame.rev,
            width: rendered.width,
            height: rendered.height,
            scale,
            layout: rendered.layout,
        };
        atomic_write(&png_path, &rendered.png)?;
        atomic_write(&layout_path, &serde_json::to_vec(&cached_layout)?)?;
        prune_cache_dir(&cache_dir);
        Ok((png_path, rendered.png, cached_layout))
    }

    /// Async wrapper for [`Self::render_frame_cached`] running on a blocking thread with concurrency limit and timeout.
    pub async fn render_frame_cached_async(
        self: &Arc<Self>,
        canvas_id: String,
        frame_id: String,
        requested_rev: Option<u64>,
        scale: f32,
        timeout: Duration,
    ) -> Result<(PathBuf, Vec<u8>, CachedLayout), CanvasError> {
        let permit = tokio::time::timeout(timeout, self.render_semaphore.clone().acquire_owned())
            .await
            .map_err(|_| CanvasError::Render(RenderError::Timeout))?
            .map_err(|_| CanvasError::Render(RenderError::Paint("renderer closed".into())))?;
        let svc = self.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            svc.render_frame_cached(&canvas_id, &frame_id, requested_rev, scale)
        });
        tokio::time::timeout(timeout, handle)
            .await
            .map_err(|_| CanvasError::Render(RenderError::Timeout))?
            .map_err(|e| {
                CanvasError::Render(RenderError::Paint(format!("render task failed: {e}")))
            })?
    }

    pub fn hit_test(
        &self,
        canvas_id: &str,
        frame_id: &str,
        x: f32,
        y: f32,
    ) -> Result<Option<String>, CanvasError> {
        if !is_safe_component(frame_id) {
            return Err(CanvasError::InvalidId(frame_id.to_string()));
        }
        if !x.is_finite() || !y.is_finite() {
            return Err(CanvasError::InvalidArgument(
                "hit_test coordinates x and y must be finite".into(),
            ));
        }
        let store_arc = self.store(canvas_id)?;
        let frame = {
            let store = store_arc.lock().unwrap_or_else(|e| e.into_inner());
            store
                .frame(frame_id)
                .ok_or_else(|| CanvasError::FrameNotFound(frame_id.to_string()))?
        };
        let width = frame.w.round().max(1.0) as u32;
        Ok(self.renderer.hit_test(&frame.html, width, x, y))
    }

    /// Async wrapper for [`Self::hit_test`] running on a blocking thread with concurrency limit and timeout.
    pub async fn hit_test_async(
        self: &Arc<Self>,
        canvas_id: String,
        frame_id: String,
        x: f32,
        y: f32,
        timeout: Duration,
    ) -> Result<Option<String>, CanvasError> {
        let permit = tokio::time::timeout(timeout, self.render_semaphore.clone().acquire_owned())
            .await
            .map_err(|_| CanvasError::Render(RenderError::Timeout))?
            .map_err(|_| CanvasError::Render(RenderError::Paint("renderer closed".into())))?;
        let svc = self.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            svc.hit_test(&canvas_id, &frame_id, x, y)
        });
        tokio::time::timeout(timeout, handle)
            .await
            .map_err(|_| CanvasError::Render(RenderError::Timeout))?
            .map_err(|e| {
                CanvasError::Render(RenderError::Paint(format!("hit_test task failed: {e}")))
            })?
    }
}

/// Emit the device-wide `/events` websocket notification for a committed canvas batch.
pub fn emit_canvas_rev_event(commit: &BatchCommit) {
    crate::session::emit_device_event(serde_json::json!({
        "type": "canvas_rev",
        "canvas_id": commit.canvas_id,
        "rev": commit.rev,
        "frame_ids": commit.frame_ids,
        "batch_id": commit.batch_id,
        "label": commit.label,
    }));
}
