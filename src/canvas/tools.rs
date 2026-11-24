//! Agent tools for reading, mutating, screenshotting, and pinning frames on the project canvas.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};

use super::links::{CanvasLink, format_markdown_link};
use super::model::{Author, Op, PinStatus};
use super::store::{CanvasService, RENDER_TIMEOUT, canvas_id_for_workspace, emit_canvas_rev_event};

/// Hard output cap for `canvas.read` and `canvas.pins` so large canvases never flood model context.
pub const MAX_CANVAS_READ_BYTES: usize = 16_000;
const MAX_CREATED_IDS_RETURNED: usize = 100;

pub const CANVAS_HTML_SUBSET_NOTE: &str = "Supported HTML/CSS subset (Blitz renderer, max frame 4000x8000 px, max HTML 512 KiB): block, inline, flex, grid, absolute/relative positioning, margin/padding/border/radius, box-shadow, linear/radial gradients, opacity, overflow hidden, basic transforms, text styling, inline SVG (no <foreignObject> or external <use>), img with raster data:image/(png|jpeg|gif|webp|avif|bmp) URIs only (no SVG data URIs). NO JavaScript, forms, sticky, subgrid, filters, animations, or external resources. Known renderer gaps: avoid <select> and <input placeholder> (use styled divs), text-overflow: ellipsis, background-clip: text and CSS filter; page CSS does not style inline SVG, so put explicit fill/font-family attributes on SVG text; set the body background on a full-height wrapper div; no non-Latin scripts (emoji are fine). Every element is addressed by `data-cv-id`; whole-frame `set_html` preserves `data-cv-id` of unchanged elements.";

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn add_canvas_tools(registry: &mut ToolRegistry) {
    registry.insert(CanvasReadTool);
    registry.insert(CanvasApplyTool);
    registry.insert(CanvasThumbnailTool);
    registry.insert(CanvasPinsTool);
}

fn active_canvas_service() -> Arc<CanvasService> {
    if let Some(d) = crate::serve::live_daemon() {
        return d.canvas.clone();
    }
    CanvasService::default_instance()
}

fn resolve_canvas_id(ctx: &ToolContext, explicit: Option<&str>) -> String {
    if let Some(cid) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return cid.to_string();
    }
    canvas_id_for_workspace(ctx.workspace_root())
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn expect_object<T>(tool: &str, arguments: Value) -> Result<T, ToolError>
where
    T: for<'de> Deserialize<'de>,
{
    if !arguments.is_object() {
        return Err(ToolError::InvalidArguments {
            tool: tool.to_string(),
        });
    }
    Ok(serde_json::from_value(arguments)?)
}

#[derive(Debug, Deserialize)]
pub(crate) struct BBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct CanvasReadArgs {
    #[serde(default)]
    pub(crate) frame_id: Option<String>,
    #[serde(default)]
    pub(crate) bbox: Option<BBox>,
    #[serde(default)]
    pub(crate) since_rev: Option<u64>,
    #[serde(default)]
    pub(crate) canvas_id: Option<String>,
}

pub struct CanvasReadTool;

#[async_trait]
impl Tool for CanvasReadTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "canvas_read".to_string(),
            description: format!(
                "Read frames or recent op-log batches from the project canvas. Filter with `frame_id`, `bbox`, or `since_rev` to stay under the output cap. {CANVAS_HTML_SUBSET_NOTE}"
            ),
            input_schema: schema(
                json!({
                    "frame_id": {"type": "string", "description": "Read one frame's full HTML and geometry by id."},
                    "bbox": {
                        "type": "object",
                        "description": "Filter frames overlapping this canvas bounding box.",
                        "properties": {
                            "x": {"type": "number"},
                            "y": {"type": "number"},
                            "w": {"type": "number"},
                            "h": {"type": "number"}
                        },
                        "required": ["x", "y", "w", "h"],
                        "additionalProperties": false
                    },
                    "since_rev": {"type": "integer", "minimum": 0, "description": "Return batches committed after this revision."},
                    "canvas_id": {"type": "string", "description": "Optional explicit canvas id; defaults to this project's canvas."}
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: CanvasReadArgs = expect_object("canvas_read", arguments)?;
        let svc = active_canvas_service();
        let cid = resolve_canvas_id(ctx, args.canvas_id.as_deref());
        let payload = tokio::task::spawn_blocking(move || {
            let store = svc
                .store(&cid)
                .map_err(|e| ToolError::msg(format!("canvas_read: {e}")))?;
            let s = store.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(fid) = args.frame_id.as_deref()
                && s.frame(fid).is_none()
            {
                let available: Vec<String> =
                    s.frames().into_iter().take(20).map(|f| f.id).collect();
                return Err(ToolError::msg(format!(
                    "canvas_read: frame `{fid}` not found in canvas `{cid}` (available frames: {})",
                    if available.is_empty() {
                        "none".to_string()
                    } else {
                        available.join(", ")
                    }
                )));
            }
            Ok(build_read_payload(&s, args, MAX_CANVAS_READ_BYTES))
        })
        .await
        .map_err(|e| ToolError::msg(format!("canvas_read join: {e}")))??;
        Ok(ToolResult::success(payload))
    }
}

pub(crate) fn build_read_payload(
    store: &super::store::CanvasStore,
    args: CanvasReadArgs,
    max_bytes: usize,
) -> Value {
    let cid = store.canvas_id().to_string();
    let rev = store.rev();
    let hint =
        "Output capped. Narrow your query with `frame_id`, `bbox` ({x,y,w,h}), or `since_rev`.";

    if let Some(fid) = args.frame_id.as_deref() {
        let Some(frame) = store.frame(fid) else {
            return json!({
                "canvas_id": cid,
                "rev": rev,
                "error": format!("frame `{fid}` not found"),
            });
        };
        let link = format_markdown_link(
            &frame.title,
            &CanvasLink::new(&cid, &frame.id, None, Some(frame.rev)),
        );
        let full = json!({
            "canvas_id": cid,
            "rev": rev,
            "frame": frame,
            "link": link,
            "truncated": false,
        });
        if serde_json::to_string(&full).map(|s| s.len()).unwrap_or(0) <= max_bytes {
            return full;
        }
        // Shrink byte budget until the serialized JSON (including escape expansion) fits under max_bytes.
        let mut budget = max_bytes.saturating_sub(400).min(frame.html.len());
        loop {
            let html_preview = truncate_utf8_bytes(&frame.html, budget);
            let candidate = json!({
                "canvas_id": cid,
                "rev": rev,
                "frame": {
                    "id": frame.id,
                    "title": frame.title,
                    "x": frame.x,
                    "y": frame.y,
                    "w": frame.w,
                    "h": frame.h,
                    "rev": frame.rev,
                    "html": html_preview,
                    "html_total_bytes": frame.html.len(),
                },
                "link": link,
                "truncated": true,
                "hint": hint,
            });
            let sz = serde_json::to_string(&candidate)
                .map(|s| s.len())
                .unwrap_or(0);
            if sz <= max_bytes || budget == 0 {
                return candidate;
            }
            let excess = sz.saturating_sub(max_bytes);
            budget = budget.saturating_sub(excess.max(64));
        }
    }

    if let Some(since) = args.since_rev {
        let batches = store.ops_since(since);
        let mut kept: Vec<Value> = Vec::new();
        let mut truncated = false;
        let mut used = 256usize;
        for b in batches {
            let full = serde_json::to_value(&b).unwrap_or(Value::Null);
            let sz = serde_json::to_string(&full).map(|s| s.len()).unwrap_or(0);
            if used + sz <= max_bytes {
                used += sz;
                kept.push(full);
            } else {
                // Fall back to a compact batch summary so a single large batch cannot exceed max_bytes.
                let summary = json!({
                    "rev": b.rev,
                    "batch_id": b.batch_id,
                    "author": b.author,
                    "ts": b.ts,
                    "label": b.label,
                    "op_count": b.ops.len(),
                    "reverts": b.reverts,
                    "ops_omitted": true,
                });
                let sum_sz = serde_json::to_string(&summary)
                    .map(|s| s.len())
                    .unwrap_or(0);
                truncated = true;
                if used + sum_sz <= max_bytes {
                    used += sum_sz;
                    kept.push(summary);
                } else {
                    break;
                }
            }
        }
        return if truncated {
            json!({
                "canvas_id": cid,
                "rev": rev,
                "since_rev": since,
                "ops": kept,
                "truncated": true,
                "hint": hint,
            })
        } else {
            json!({
                "canvas_id": cid,
                "rev": rev,
                "since_rev": since,
                "ops": kept,
                "truncated": false,
            })
        };
    }

    let mut frames = store.frames();
    if let Some(bbox) = args.bbox {
        frames.retain(|f| {
            f.x < bbox.x + bbox.w
                && f.x + f.w > bbox.x
                && f.y < bbox.y + bbox.h
                && f.y + f.h > bbox.y
        });
    }

    let mut out = Vec::new();
    let mut truncated = false;
    let mut used = 256usize;
    let total = frames.len();
    for f in frames {
        let entry = json!({
            "id": f.id,
            "title": f.title,
            "x": f.x,
            "y": f.y,
            "w": f.w,
            "h": f.h,
            "rev": f.rev,
            "html_bytes": f.html.len(),
            "html": f.html,
            "link": format_markdown_link(&f.title, &CanvasLink::new(&cid, &f.id, None, Some(f.rev))),
        });
        let sz = serde_json::to_string(&entry).map(|s| s.len()).unwrap_or(0);
        if used + sz > max_bytes {
            // Fall back to metadata-only summary when full HTML doesn't fit.
            let summary = json!({
                "id": f.id,
                "title": f.title,
                "x": f.x,
                "y": f.y,
                "w": f.w,
                "h": f.h,
                "rev": f.rev,
                "html_bytes": f.html.len(),
                "link": format_markdown_link(&f.title, &CanvasLink::new(&cid, &f.id, None, Some(f.rev))),
            });
            let sum_sz = serde_json::to_string(&summary)
                .map(|s| s.len())
                .unwrap_or(0);
            truncated = true;
            if used + sum_sz <= max_bytes {
                used += sum_sz;
                out.push(summary);
            } else {
                break;
            }
        } else {
            used += sz;
            out.push(entry);
        }
    }

    if truncated {
        json!({
            "canvas_id": cid,
            "rev": rev,
            "total_frames": total,
            "frames": out,
            "truncated": true,
            "hint": hint,
        })
    } else {
        json!({
            "canvas_id": cid,
            "rev": rev,
            "total_frames": total,
            "frames": out,
            "truncated": false,
        })
    }
}

#[derive(Debug, Deserialize)]
struct CanvasApplyArgs {
    label: String,
    ops: Vec<Op>,
    #[serde(default)]
    canvas_id: Option<String>,
}

pub struct CanvasApplyTool;

#[async_trait]
impl Tool for CanvasApplyTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "canvas_apply".to_string(),
            description: format!(
                "Apply an atomic, named batch of canvas ops (`create_frame`, `set_html`, `dom`, `move_frame`, `delete_frame`). Returns the new revision, created element/frame ids, and shareable markdown `[title](canvas:...)` link(s). {CANVAS_HTML_SUBSET_NOTE}"
            ),
            input_schema: schema(
                json!({
                    "label": {"type": "string", "description": "Short human-readable step label shown in history and revert UI."},
                    "ops": {
                        "type": "array",
                        "description": "Atomic ops (max 200). `op`: create_frame{title,x?,y?,w?(<=4000),h?(<=8000),html?,id?}, set_html{frame_id,html}, dom{frame_id,dom_op:patch{el_id,attrs?,style?,text?}|insert{parent_id,index,html}|remove{el_id}|move{el_id,parent_id,index}}, move_frame{frame_id,x,y,w?,h?,title?}, delete_frame{frame_id}.",
                        "items": {"type": "object"}
                    },
                    "canvas_id": {"type": "string", "description": "Optional explicit canvas id; defaults to this project's canvas."}
                }),
                &["label", "ops"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: CanvasApplyArgs = expect_object("canvas_apply", arguments)?;
        let svc = active_canvas_service();
        let cid = resolve_canvas_id(ctx, args.canvas_id.as_deref());
        let author = Author::agent(ctx.agent_id().unwrap_or(ctx.owner()));
        let session = ctx.durable_session_id().map(str::to_string);
        let commit = tokio::task::spawn_blocking(move || {
            let store = svc
                .store(&cid)
                .map_err(|e| ToolError::msg(format!("canvas_apply: {e}")))?;
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            s.apply_batch(args.label, args.ops, author, session)
                .map_err(|e| ToolError::msg(format!("canvas_apply: {e}")))
        })
        .await
        .map_err(|e| ToolError::msg(format!("canvas_apply join: {e}")))??;
        emit_canvas_rev_event(&commit);
        let primary_link = commit.links.first().cloned();
        let total_created = commit.created_ids.len();
        let created_truncated = total_created > MAX_CREATED_IDS_RETURNED;
        let created_ids: Vec<String> = commit
            .created_ids
            .into_iter()
            .take(MAX_CREATED_IDS_RETURNED)
            .collect();
        let mut resp = json!({
            "canvas_id": commit.canvas_id,
            "rev": commit.rev,
            "batch_id": commit.batch_id,
            "created_ids": created_ids,
            "links": commit.links,
            "link": primary_link,
        });
        if created_truncated {
            resp["created_ids_total"] = json!(total_created);
            resp["created_ids_truncated"] = json!(true);
        }
        Ok(ToolResult::success(resp))
    }
}

#[derive(Debug, Deserialize)]
struct CanvasThumbnailArgs {
    frame_id: String,
    #[serde(default)]
    scale: Option<f32>,
    #[serde(default)]
    rev: Option<u64>,
    #[serde(default)]
    canvas_id: Option<String>,
}

pub struct CanvasThumbnailTool;

#[async_trait]
impl Tool for CanvasThumbnailTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "canvas_thumbnail".to_string(),
            description: "Render a canvas frame to PNG and attach the image to your context so you can visually inspect it (cached by frame, rev, and scale).".to_string(),
            input_schema: schema(
                json!({
                    "frame_id": {"type": "string", "description": "Frame id to render."},
                    "scale": {"type": "number", "minimum": 0.25, "maximum": 3.0, "description": "Render scale factor (default 1.0, range 0.25..=3.0)."},
                    "rev": {"type": "integer", "minimum": 1, "description": "Optional historical revision to render."},
                    "canvas_id": {"type": "string", "description": "Optional explicit canvas id; defaults to this project's canvas."}
                }),
                &["frame_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: CanvasThumbnailArgs = expect_object("canvas_thumbnail", arguments)?;
        let svc = active_canvas_service();
        let cid = resolve_canvas_id(ctx, args.canvas_id.as_deref());
        let scale = args.scale.unwrap_or(1.0);
        let frame_id = args.frame_id.clone();
        let rev = args.rev;
        let (path, png, layout) = svc
            .render_frame_cached_async(cid, frame_id, rev, scale, RENDER_TIMEOUT)
            .await
            .map_err(|e| ToolError::msg(format!("canvas_thumbnail: {e}")))?;

        Ok(ToolResult::success(json!({
            "path": path.display().to_string(),
            "mime": "image/png",
            "size_bytes": png.len(),
            "width": layout.width,
            "height": layout.height,
            "canvas_id": layout.canvas_id,
            "frame_id": layout.frame_id,
            "rev": layout.rev,
            "scale": layout.scale,
        })))
    }
}

#[derive(Debug, Deserialize)]
struct CanvasPinsArgs {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    frame_id: Option<String>,
    #[serde(default)]
    status: Option<PinStatus>,
    #[serde(default)]
    pin_id: Option<String>,
    #[serde(default)]
    reply: Option<String>,
    #[serde(default)]
    canvas_id: Option<String>,
}

pub struct CanvasPinsTool;

#[async_trait]
impl Tool for CanvasPinsTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "canvas_pins".to_string(),
            description:
                "List, reply to, or resolve feedback pins anchored to elements on canvas frames."
                    .to_string(),
            input_schema: schema(
                json!({
                    "action": {"type": "string", "enum": ["list", "reply", "resolve"], "description": "Pin operation to perform."},
                    "frame_id": {"type": "string", "description": "Optional frame id filter for `list`."},
                    "status": {"type": "string", "enum": ["open", "resolved"], "description": "Optional status filter for `list`."},
                    "pin_id": {"type": "string", "description": "Pin id required for `reply` and `resolve`."},
                    "reply": {"type": "string", "description": "Reply text for `reply` (and optional closing note for `resolve`)."},
                    "canvas_id": {"type": "string", "description": "Optional explicit canvas id; defaults to this project's canvas."}
                }),
                &["action"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: CanvasPinsArgs = expect_object("canvas_pins", arguments)?;
        let op = args
            .action
            .as_deref()
            .or(args.op.as_deref())
            .ok_or_else(|| {
                ToolError::msg("canvas_pins requires `action` (list, reply, resolve)")
            })?;
        let svc = active_canvas_service();
        let cid = resolve_canvas_id(ctx, args.canvas_id.as_deref());
        let op = op.to_string();
        let frame_id = args.frame_id;
        let status = args.status;
        let pin_id = args.pin_id;
        let reply = args.reply;
        let payload = tokio::task::spawn_blocking(move || {
            let store = svc
                .store(&cid)
                .map_err(|e| ToolError::msg(format!("canvas_pins: {e}")))?;
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            match op.as_str() {
                "list" => {
                    let all_pins = s.pins(frame_id.as_deref(), status);
                    let total = all_pins.len();
                    let mut kept = Vec::new();
                    let mut used = 256usize;
                    let mut truncated = false;
                    for p in all_pins {
                        let sz = serde_json::to_string(&p).map(|v| v.len()).unwrap_or(0);
                        if used + sz > MAX_CANVAS_READ_BYTES {
                            truncated = true;
                            break;
                        }
                        used += sz;
                        kept.push(p);
                    }
                    let mut out = json!({
                        "canvas_id": s.canvas_id(),
                        "total_pins": total,
                        "pins": kept,
                        "truncated": truncated,
                    });
                    if truncated {
                        out["hint"] = json!(
                            "Output capped. Filter pins with `frame_id` or `status` (`open`|`resolved`)."
                        );
                    }
                    Ok(out)
                }
                "reply" => {
                    let pin_id = pin_id
                        .as_deref()
                        .filter(|p| !p.is_empty())
                        .ok_or_else(|| ToolError::msg("canvas_pins reply requires `pin_id`"))?;
                    let reply = reply
                        .filter(|r| !r.trim().is_empty())
                        .ok_or_else(|| {
                            ToolError::msg("canvas_pins reply requires non-empty `reply`")
                        })?;
                    let pin = s
                        .update_pin(pin_id, None, Some(reply), None)
                        .map_err(|e| ToolError::msg(format!("canvas_pins: {e}")))?;
                    Ok(json!({ "pin": pin }))
                }
                "resolve" => {
                    let pin_id = pin_id
                        .as_deref()
                        .filter(|p| !p.is_empty())
                        .ok_or_else(|| ToolError::msg("canvas_pins resolve requires `pin_id`"))?;
                    let pin = s
                        .update_pin(pin_id, Some(PinStatus::Resolved), reply, None)
                        .map_err(|e| ToolError::msg(format!("canvas_pins: {e}")))?;
                    Ok(json!({ "pin": pin }))
                }
                other => Err(ToolError::msg(format!(
                    "canvas_pins action must be list, reply, or resolve (got `{other}`)"
                ))),
            }
        })
        .await
        .map_err(|e| ToolError::msg(format!("canvas_pins join: {e}")))??;
        Ok(ToolResult::success(payload))
    }
}
