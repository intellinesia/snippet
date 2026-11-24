use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use anyrender::{PaintScene as _, render_to_buffer};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{BaseDocument, DocumentConfig, Node, NodeId, util::Color};
use blitz_html::HtmlDocument;
use blitz_paint::paint_scene;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, Viewport};
use parley::fontique::{Blob, Collection, CollectionOptions, GenericFamily, SourceCache};
use parley::{FontContext, PositionedLayoutItem};
use peniko::Fill;
use peniko::kurbo::Rect;

pub const MAX_WIDTH: u32 = 4000;
pub const MAX_HEIGHT: u32 = 8000;
pub const MAX_RENDER_EDGE: u32 = 8192;
pub const MAX_RENDER_PIXELS: u64 = 33_554_432;
pub const MIN_SCALE: f32 = 0.25;
pub const MAX_SCALE: f32 = 3.0;
pub const MAX_HTML_BYTES: usize = 512 * 1024;
const MAX_DATA_URI_BYTES: usize = 4 * 1024 * 1024;
const AUTO_VIEWPORT_HEIGHT: u32 = 800;

static INTER: &[u8] = include_bytes!("../fonts/Inter.ttf");
static INTER_ITALIC: &[u8] = include_bytes!("../fonts/Inter-Italic.ttf");
static EMOJI: &[u8] = include_bytes!("../fonts/TwemojiMozilla.ttf");
static SERIF: &[u8] = include_bytes!("../fonts/DejaVuSerif.ttf");
static SERIF_BOLD: &[u8] = include_bytes!("../fonts/DejaVuSerif-Bold.ttf");
static MONO: &[u8] = include_bytes!("../fonts/JetBrainsMono.ttf");
static MONO_ITALIC: &[u8] = include_bytes!("../fonts/JetBrainsMono-Italic.ttf");

#[derive(Debug, Clone, PartialEq)]
pub struct ElementRect {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub tag: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Rendered {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub layout: Vec<ElementRect>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderError {
    Parse(String),
    Layout(String),
    Paint(String),
    TooLarge,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Parse(e) => write!(f, "parse error: {e}"),
            RenderError::Layout(e) => write!(f, "layout error: {e}"),
            RenderError::Paint(e) => write!(f, "paint error: {e}"),
            RenderError::TooLarge => write!(f, "frame exceeds {MAX_WIDTH}x{MAX_HEIGHT} logical px"),
        }
    }
}

impl std::error::Error for RenderError {}

struct DataOnlyNet;

fn is_allowed_data_mime(mime: &data_url::mime::Mime) -> bool {
    mime.type_.eq_ignore_ascii_case("image")
        && matches!(
            mime.subtype.to_ascii_lowercase().as_str(),
            "png" | "jpeg" | "jpg" | "gif" | "webp" | "avif" | "bmp"
        )
}

impl NetProvider for DataOnlyNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        if request.url.scheme() != "data" {
            return;
        }
        let url = request.url.as_str();
        if url.len() > MAX_DATA_URI_BYTES * 2 {
            return;
        }
        let Ok(parsed) = data_url::DataUrl::process(url) else {
            return;
        };
        if !is_allowed_data_mime(parsed.mime_type()) {
            return;
        }
        let Ok((bytes, _)) = parsed.decode_to_vec() else {
            return;
        };
        if bytes.len() > MAX_DATA_URI_BYTES {
            return;
        }
        handler.bytes(url.to_string(), Bytes::from(bytes));
    }

    fn is_noop(&self) -> bool {
        true
    }
}

fn font_context() -> FontContext {
    static BASE: OnceLock<FontContext> = OnceLock::new();
    BASE.get_or_init(|| {
        let mut ctx = FontContext {
            source_cache: SourceCache::new_shared(),
            collection: Collection::new(CollectionOptions {
                shared: false,
                system_fonts: false,
            }),
        };
        let mut register = |data: &'static [u8]| {
            ctx.collection
                .register_fonts(Blob::new(Arc::new(data) as _), None)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        };
        let mut sans = register(INTER);
        sans.extend(register(INTER_ITALIC));
        sans.dedup();
        let emoji = register(EMOJI);
        let mut serif = register(SERIF);
        serif.extend(register(SERIF_BOLD));
        serif.dedup();
        let mut mono = register(MONO);
        mono.extend(register(MONO_ITALIC));
        mono.dedup();
        register(blitz_dom::BULLET_FONT);
        use GenericFamily::*;
        for g in [
            SansSerif,
            SystemUi,
            UiSansSerif,
            UiRounded,
            Cursive,
            Fantasy,
            Math,
            FangSong,
        ] {
            ctx.collection.set_generic_families(g, sans.iter().copied());
        }
        ctx.collection
            .set_generic_families(Emoji, emoji.iter().copied());
        ctx.collection
            .set_generic_families(Serif, serif.iter().copied());
        ctx.collection
            .set_generic_families(UiSerif, serif.iter().copied());
        ctx.collection
            .set_generic_families(Monospace, mono.iter().copied());
        ctx.collection
            .set_generic_families(UiMonospace, mono.iter().copied());
        ctx
    })
    .clone()
}

const UA_EXTRA: &str = "html { font-family: sans-serif; }";

struct Laid {
    doc: HtmlDocument,
    content_height: u32,
}

fn layout_doc(html: &str, width: u32, height: u32, scale: f32) -> Result<Laid, RenderError> {
    if width == 0 {
        return Err(RenderError::Layout("width must be > 0".into()));
    }
    if width > MAX_WIDTH || height > MAX_HEIGHT {
        return Err(RenderError::TooLarge);
    }
    if html.trim().is_empty() {
        return Err(RenderError::Parse("empty html".into()));
    }
    let vh = if height == 0 {
        AUTO_VIEWPORT_HEIGHT
    } else {
        height
    };
    let phys = |v: u32| ((v as f32) * scale).round().max(1.0) as u32;
    let html = inject_base_style(html);
    let net: Arc<dyn NetProvider> = Arc::new(DataOnlyNet);
    let mut doc = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            viewport: Some(Viewport::new(
                phys(width),
                phys(vh),
                scale,
                ColorScheme::Light,
            )),
            net_provider: Some(net),
            font_ctx: Some(font_context()),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    // Second pass is only needed when synchronous data: URI images load during the first resolve.
    if html.contains("data:") {
        doc.resolve(0.0);
    }

    let content_height = {
        let base: &BaseDocument = doc.as_ref();
        let root = base.root_element();
        let l = root.final_layout();
        l.size.height.max(root.scroll_height()).ceil()
    };
    if !content_height.is_finite() {
        return Err(RenderError::Layout("non-finite content height".into()));
    }
    let content_height = (content_height as u32).clamp(1, MAX_HEIGHT);
    Ok(Laid {
        doc,
        content_height,
    })
}

fn inject_base_style(html: &str) -> String {
    let style = format!("<style>{UA_EXTRA}</style>");
    let lower = html.to_ascii_lowercase();
    if let Some(i) = lower.find("<head>") {
        let at = i + "<head>".len();
        format!("{}{}{}", &html[..at], style, &html[at..])
    } else {
        format!("{style}{html}")
    }
}

pub fn render(html: &str, width: u32, height: u32, scale: f32) -> Result<Rendered, RenderError> {
    if html.len() > MAX_HTML_BYTES {
        return Err(RenderError::TooLarge);
    }
    if !scale.is_finite() || !(MIN_SCALE..=MAX_SCALE).contains(&scale) {
        return Err(RenderError::Layout(format!(
            "scale must be between {MIN_SCALE} and {MAX_SCALE}"
        )));
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Laid {
            mut doc,
            content_height,
        } = layout_doc(html, width, height, scale)?;
        let out_h = if height == 0 { content_height } else { height };
        let pw = ((width as f64) * (scale as f64)).round();
        let ph = ((out_h as f64) * (scale as f64)).round();
        if pw < 1.0
            || ph < 1.0
            || pw > MAX_RENDER_EDGE as f64
            || ph > MAX_RENDER_EDGE as f64
            || (pw as u64).saturating_mul(ph as u64) > MAX_RENDER_PIXELS
        {
            return Err(RenderError::TooLarge);
        }
        let (pw, ph) = (pw as u32, ph as u32);
        let layout = collect_layout(doc.as_ref(), scale);
        let buf = render_to_buffer::<VelloCpuImageRenderer, _>(
            |scene| {
                scene.fill(
                    Fill::NonZero,
                    Default::default(),
                    Color::WHITE,
                    Default::default(),
                    &Rect::new(0.0, 0.0, pw as f64, ph as f64),
                );
                paint_scene(scene, doc.as_mut(), scale as f64, pw, ph, 0, 0);
            },
            pw,
            ph,
        );
        let png = encode_png(&buf, pw, ph)?;
        Ok(Rendered {
            png,
            width: pw,
            height: ph,
            layout,
        })
    }))
    .unwrap_or_else(|e| Err(RenderError::Paint(panic_message(e))))
}

pub fn hit_test(html: &str, width: u32, x: f32, y: f32) -> Option<String> {
    if html.len() > MAX_HTML_BYTES || !x.is_finite() || !y.is_finite() {
        return None;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let laid = layout_doc(html, width, 0, 1.0).ok()?;
        let base: &BaseDocument = laid.doc.as_ref();
        if let Some(hit) = base.hit(x, y) {
            let mut cur = Some(hit.node_id);
            while let Some(id) = cur {
                let node = base.get_node(id)?;
                if let Some(cv) = cv_id(node) {
                    return Some(cv);
                }
                cur = node.parent;
            }
        }
        collect_layout(base, 1.0)
            .into_iter()
            .rfind(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
            .map(|r| r.id)
    }))
    .ok()
    .flatten()
}

fn cv_id(node: &Node) -> Option<String> {
    node.attrs()?
        .iter()
        .find(|a| &*a.name.local == "data-cv-id")
        .map(|a| a.value.to_string())
}

type Bounds = (f32, f32, f32, f32);

fn union(a: Option<Bounds>, b: Bounds) -> Bounds {
    match a {
        None => b,
        Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
    }
}

fn inline_bounds(doc: &BaseDocument, scale: f32) -> HashMap<NodeId, Bounds> {
    let mut map: HashMap<NodeId, Bounds> = HashMap::new();
    let mut stack = vec![doc.root_element().id];
    while let Some(id) = stack.pop() {
        let Some(node) = doc.get_node(id) else {
            continue;
        };
        stack.extend(node.children.iter().copied());
        let Some(text) = node
            .element_data()
            .and_then(|e| e.inline_layout_data.as_deref())
        else {
            continue;
        };
        let l = node.final_layout();
        let origin = node.absolute_position(0.0, 0.0);
        let ox = origin.x + l.border.left + l.padding.left;
        let oy = origin.y + l.border.top + l.padding.top + text.block_offset;
        for line in text.layout.lines() {
            let m = line.metrics();
            let (top, bottom) = (m.block_min_coord / scale, m.block_max_coord / scale);
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                let (x0, x1) = (run.offset() / scale, (run.offset() + run.advance()) / scale);
                let b = (ox + x0, oy + top, ox + x1, oy + bottom);
                let mut cur = Some(run.style().brush.id);
                while let Some(c) = cur {
                    if c == id {
                        break;
                    }
                    map.insert(c, union(map.get(&c).copied(), b));
                    cur = doc.get_node(c).and_then(|n| n.parent);
                }
            }
        }
    }
    map
}

fn collect_layout(doc: &BaseDocument, scale: f32) -> Vec<ElementRect> {
    let inline = inline_bounds(doc, scale);
    let mut out = Vec::new();
    let mut stack = vec![doc.root_element().id];
    while let Some(id) = stack.pop() {
        let Some(node) = doc.get_node(id) else {
            continue;
        };
        if let Some(cv) = cv_id(node) {
            let p = node.absolute_position(0.0, 0.0);
            let size = node.final_layout().size;
            let (x, y, w, h) = match inline.get(&id) {
                Some(b) if size.width == 0.0 && size.height == 0.0 => {
                    (b.0, b.1, b.2 - b.0, b.3 - b.1)
                }
                _ => (p.x, p.y, size.width, size.height),
            };
            out.push(ElementRect {
                id: cv,
                x,
                y,
                w,
                h,
                tag: node
                    .element_data()
                    .map(|e| e.name.local.to_string())
                    .unwrap_or_default(),
                text: direct_text(node),
            });
        }
        for child in node.children.iter().rev() {
            stack.push(*child);
        }
    }
    out
}

fn direct_text(node: &Node) -> Option<String> {
    let mut s = String::new();
    for child in &node.children {
        if let Some(n) = node.with(*child).text_data() {
            s.push_str(&n.content);
            s.push(' ');
        }
    }
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed.chars().take(60).collect())
    }
}

fn encode_png(buf: &[u8], w: u32, h: u32) -> Result<Vec<u8>, RenderError> {
    let mut out = Vec::with_capacity(buf.len() / 4);
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc
            .write_header()
            .map_err(|e| RenderError::Paint(e.to_string()))?;
        wr.write_image_data(buf)
            .map_err(|e| RenderError::Paint(e.to_string()))?;
        wr.finish().map_err(|e| RenderError::Paint(e.to_string()))?;
    }
    Ok(out)
}

fn panic_message(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic during paint".into())
}
