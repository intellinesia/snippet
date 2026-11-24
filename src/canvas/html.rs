use std::collections::{HashMap, HashSet, VecDeque};

use html5ever::tendril::TendrilSink;
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

const ID_ATTR: &str = "data-cv-id";
const ROOT_ID: &str = "body";

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
const RAW_TEXT: &[&str] = &[
    "style",
    "script",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "plaintext",
    "noscript",
];
pub const MAX_HTML_BYTES: usize = 512 * 1024;
pub const MAX_DOM_DEPTH: usize = 64;
pub const MAX_DOM_NODES: usize = 10_000;

const DROP_TAGS: &[&str] = &[
    "script",
    "iframe",
    "object",
    "embed",
    "base",
    "frame",
    "frameset",
    "applet",
    "link",
    "foreignobject",
    "noscript",
    "template",
    "noembed",
    "noframes",
    "plaintext",
    "xmp",
    "portal",
    "fencedframe",
    "bgsound",
    "marquee",
    "animate",
    "animatemotion",
    "animatetransform",
    "set",
    "handler",
    "listener",
    "mpath",
];
const STRIP_ATTRS: &[&str] = &[
    "action",
    "formaction",
    "srcdoc",
    "xml:base",
    "dynsrc",
    "lowsrc",
    "ping",
    "manifest",
    "longdesc",
    "background",
    "data",
    "cite",
    "is",
    "attributename",
];
const AUTO_CLOSE_SAME_TAGS: &[&str] = &["p", "li", "dt", "dd", "tr", "td", "th", "option"];
const NO_ID_TAGS: &[&str] = &["html", "head", "body", "style", "meta"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlError {
    NotFound(String),
    BadParent(String),
    Parse(String),
    TooLarge(String),
    TooDeep(usize),
}

impl std::fmt::Display for HtmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HtmlError::NotFound(id) => write!(
                f,
                "element `{id}` not found; read the frame with canvas_read to inspect current data-cv-id values"
            ),
            HtmlError::BadParent(reason) => write!(f, "invalid parent or structure: {reason}"),
            HtmlError::Parse(m) => write!(f, "parse error: {m}"),
            HtmlError::TooLarge(m) => write!(f, "html too large: {m}"),
            HtmlError::TooDeep(max) => {
                write!(f, "html nesting depth exceeds maximum of {max} levels")
            }
        }
    }
}

impl std::error::Error for HtmlError {}

#[derive(Debug, Clone)]
pub struct IdGen {
    used: HashSet<String>,
    counter: u64,
}

impl IdGen {
    pub fn new(seed_existing: &[String]) -> Self {
        Self {
            used: seed_existing.iter().cloned().collect(),
            counter: 0,
        }
    }

    pub fn reserve(&mut self, id: &str) {
        self.used.insert(id.to_string());
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> String {
        loop {
            self.counter += 1;
            let id = format!("e_{}", base36(self.counter));
            if self.used.insert(id.clone()) {
                return id;
            }
        }
    }
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    pub html: String,
    pub ids_kept: usize,
    pub ids_new: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "dom_op", rename_all = "snake_case")]
pub enum DomOp {
    Patch {
        el_id: String,
        #[serde(default)]
        attrs: Vec<(String, Option<String>)>,
        #[serde(default)]
        style: Vec<(String, Option<String>)>,
        #[serde(default)]
        text: Option<String>,
    },
    Insert {
        parent_id: String,
        index: usize,
        html: String,
    },
    Remove {
        el_id: String,
    },
    Move {
        el_id: String,
        parent_id: String,
        index: usize,
    },
}

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    El(El),
}

#[derive(Debug, Clone)]
struct El {
    tag: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
}

impl El {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn set_attr(&mut self, name: &str, value: String) {
        match self.attrs.iter_mut().find(|(k, _)| k == name) {
            Some(slot) => slot.1 = value,
            None => self.attrs.push((name.to_string(), value)),
        }
    }

    fn remove_attr(&mut self, name: &str) {
        self.attrs.retain(|(k, _)| k != name);
    }

    fn direct_text(&self) -> String {
        self.children
            .iter()
            .filter_map(|c| match c {
                Node::Text(t) => Some(t.as_str()),
                Node::El(_) => None,
            })
            .collect()
    }
}

/// Pre-parse guard preventing stack overflows in html5ever's recursive RcDom drop
/// and bounding memory on hostile inputs.
pub fn check_raw_html(html: &str) -> Result<(), HtmlError> {
    if html.len() > MAX_HTML_BYTES {
        return Err(HtmlError::TooLarge(format!(
            "{} bytes exceeds limit of {MAX_HTML_BYTES} bytes",
            html.len()
        )));
    }
    let bytes = html.as_bytes();
    let mut i = 0usize;
    let mut stack: Vec<String> = Vec::new();
    let mut tags = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            if let Some(end) = html[i + 4..].find("-->") {
                i = i + 4 + end + 3;
            } else {
                break;
            }
            continue;
        }
        if i + 1 < bytes.len() && (bytes[i + 1] == b'!' || bytes[i + 1] == b'?') {
            if let Some(end) = html[i + 2..].find('>') {
                i = i + 2 + end + 1;
            } else {
                break;
            }
            continue;
        }
        let closing = i + 1 < bytes.len() && bytes[i + 1] == b'/';
        let name_start = if closing { i + 2 } else { i + 1 };
        let mut name_end = name_start;
        while name_end < bytes.len()
            && (bytes[name_end].is_ascii_alphanumeric()
                || matches!(bytes[name_end], b'-' | b'_' | b':'))
        {
            name_end += 1;
        }
        if name_end == name_start {
            i += 1;
            continue;
        }
        let tag = html[name_start..name_end].to_ascii_lowercase();

        // Advance to the end of the tag, respecting quoted attribute values.
        let mut j = name_end;
        let mut quote: Option<u8> = None;
        while j < bytes.len() {
            let b = bytes[j];
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                }
            } else if b == b'"' || b == b'\'' {
                quote = Some(b);
            } else if b == b'>' {
                break;
            }
            j += 1;
        }
        let self_closing = !closing && j > name_end && bytes[j - 1] == b'/';
        i = (j + 1).min(bytes.len());

        tags += 1;
        if tags > MAX_DOM_NODES * 2 {
            return Err(HtmlError::TooLarge(format!(
                "tag count exceeds limit of {}",
                MAX_DOM_NODES * 2
            )));
        }

        if closing {
            if let Some(pos) = stack.iter().rposition(|t| t == &tag) {
                stack.truncate(pos);
            }
            continue;
        }

        if RAW_TEXT.contains(&tag.as_str()) {
            let close_pat = format!("</{tag}");
            let lower_rest = html[i..].to_ascii_lowercase();
            if let Some(rel) = lower_rest.find(&close_pat) {
                let after = i + rel + close_pat.len();
                if let Some(gt) = html[after..].find('>') {
                    i = after + gt + 1;
                } else {
                    i = bytes.len();
                }
            } else {
                i = bytes.len();
            }
            continue;
        }

        if self_closing || VOID.contains(&tag.as_str()) {
            continue;
        }

        if AUTO_CLOSE_SAME_TAGS.contains(&tag.as_str()) && stack.last().is_some_and(|t| t == &tag) {
            continue;
        }
        stack.push(tag);
        if stack.len() > MAX_DOM_DEPTH + 16 {
            return Err(HtmlError::TooDeep(MAX_DOM_DEPTH));
        }
    }
    Ok(())
}

fn convert(
    handle: &Handle,
    depth: usize,
    node_count: &mut usize,
    out: &mut Vec<Node>,
) -> Result<(), HtmlError> {
    if depth > MAX_DOM_DEPTH + 4 {
        return Err(HtmlError::TooDeep(MAX_DOM_DEPTH));
    }
    *node_count += 1;
    if *node_count > MAX_DOM_NODES {
        return Err(HtmlError::TooLarge(format!(
            "DOM node count exceeds limit of {MAX_DOM_NODES}"
        )));
    }
    match &handle.data {
        NodeData::Text { contents } => out.push(Node::Text(contents.borrow().to_string())),
        NodeData::Element { name, attrs, .. } => {
            let attrs = attrs
                .borrow()
                .iter()
                .map(|a| {
                    let local: &str = a.name.local.as_ref();
                    let key = match &a.name.prefix {
                        Some(p) => format!("{}:{}", AsRef::<str>::as_ref(p), local),
                        None => local.to_string(),
                    };
                    (key, a.value.to_string())
                })
                .collect();
            let mut children = Vec::new();
            for child in handle.children.borrow().iter() {
                convert(child, depth + 1, node_count, &mut children)?;
            }
            out.push(Node::El(El {
                tag: AsRef::<str>::as_ref(&name.local).to_string(),
                attrs,
                children,
            }));
        }
        _ => {}
    }
    Ok(())
}

fn check_node_depth(nodes: &[Node], depth: usize) -> Result<(), HtmlError> {
    if depth > MAX_DOM_DEPTH {
        return Err(HtmlError::TooDeep(MAX_DOM_DEPTH));
    }
    for n in nodes {
        if let Node::El(el) = n {
            check_node_depth(&el.children, depth + 1)?;
        }
    }
    Ok(())
}

fn parse_roots(html: &str) -> Result<Vec<Node>, HtmlError> {
    check_raw_html(html)?;
    let dom = parse_document(RcDom::default(), ParseOpts::default()).one(html);
    let mut top = Vec::new();
    let mut node_count = 0usize;
    for child in dom.document.children.borrow().iter() {
        convert(child, 0, &mut node_count, &mut top)?;
    }
    let mut roots = Vec::new();
    for node in top {
        let Node::El(html_el) = node else { continue };
        if html_el.tag != "html" {
            continue;
        }
        let mut body = Vec::new();
        for part in html_el.children {
            let Node::El(part) = part else { continue };
            match part.tag.as_str() {
                "head" => {
                    for h in part.children {
                        if let Node::El(e) = &h
                            && (e.tag == "style" || e.tag == "meta")
                        {
                            roots.push(h);
                        }
                    }
                }
                "body" => body = part.children,
                _ => {}
            }
        }
        roots.extend(body);
    }
    check_node_depth(&roots, 0)?;
    Ok(roots)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UrlContext {
    /// SVG `<use href>` — strictly local `#id` fragments only.
    FragmentOnly,
    /// Anchor/link `href` — strictly local `#fragment` only (no javascript:, vbscript:, data:, external).
    Link,
    /// Image `src`, `srcset`, `poster`, CSS `url()`, SVG `<image>` — raster `data:image/*` or safe `/canvas/assets/*`.
    Image,
}

fn is_safe_asset_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/canvas/assets/") else {
        return false;
    };
    if rest.is_empty() || rest.contains('%') || rest.contains('\\') || rest.contains(',') {
        return false;
    }
    rest.split('/').all(super::links::is_safe_component)
}

fn is_safe_raster_data_uri(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix("data:") else {
        return false;
    };
    const ALLOWED_IMAGE_MIMES: &[&str] = &[
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/gif",
        "image/webp",
        "image/avif",
        "image/bmp",
    ];
    ALLOWED_IMAGE_MIMES.iter().any(|mime| {
        rest.strip_prefix(mime)
            .is_some_and(|after| after.starts_with(';') || after.starts_with(','))
    })
}

fn url_allowed(raw: &str, ctx: UrlContext) -> bool {
    if raw.chars().any(char::is_control) {
        return false;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    match ctx {
        UrlContext::FragmentOnly => {
            if let Some(frag) = trimmed.strip_prefix('#') {
                !frag.is_empty()
                    && frag.len() <= 128
                    && frag
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
            } else {
                false
            }
        }
        UrlContext::Link => {
            if let Some(frag) = trimmed.strip_prefix('#') {
                !frag.is_empty() && !frag.chars().any(char::is_whitespace)
            } else {
                false
            }
        }
        UrlContext::Image => {
            if lower.starts_with("data:") {
                is_safe_raster_data_uri(&lower) && !trimmed.chars().any(char::is_whitespace)
            } else if trimmed.starts_with("/canvas/assets/") {
                is_safe_asset_path(trimmed)
            } else {
                false
            }
        }
    }
}

fn is_srcset_descriptor(token: &str) -> bool {
    if token.len() < 2 {
        return false;
    }
    let (num, suffix) = token.split_at(token.len() - 1);
    match suffix {
        "w" => !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()),
        "x" => {
            let mut dots = 0usize;
            let mut digits = 0usize;
            for c in num.chars() {
                if c.is_ascii_digit() {
                    digits += 1;
                } else if c == '.' {
                    dots += 1;
                } else {
                    return false;
                }
            }
            digits > 0 && dots <= 1
        }
        _ => false,
    }
}

fn srcset_allowed(value: &str) -> bool {
    if value.chars().any(char::is_control) {
        return false;
    }
    let bytes = value.as_bytes();
    let mut i = 0usize;
    let mut count = 0usize;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let url_start = i;
        if value[i..].len() >= 5 && value[i..i + 5].eq_ignore_ascii_case("data:") {
            // A data: URI contains one comma separating header and payload.
            let Some(comma_rel) = value[i..].find(',') else {
                return false;
            };
            i += comma_rel + 1;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b',' {
                i += 1;
            }
        } else {
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b',' {
                i += 1;
            }
        }
        let url = &value[url_start..i];
        if !url_allowed(url, UrlContext::Image) {
            return false;
        }
        count += 1;

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] != b',' {
            let desc_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b',' {
                i += 1;
            }
            if !is_srcset_descriptor(&value[desc_start..i]) {
                return false;
            }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        if i < bytes.len() {
            if bytes[i] != b',' {
                return false;
            }
            i += 1;
            // Reject trailing comma with no following candidate.
            if value[i..].trim().is_empty() {
                return false;
            }
        }
    }
    count > 0
}

fn css_unescape(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            break;
        }
        if chars[i].is_ascii_hexdigit() {
            let mut hex = String::new();
            while i < chars.len() && hex.len() < 6 && chars[i].is_ascii_hexdigit() {
                hex.push(chars[i]);
                i += 1;
            }
            if i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let c = u32::from_str_radix(&hex, 16)
                .ok()
                .and_then(char::from_u32)
                .filter(|&c| c != '\0')
                .unwrap_or('\u{fffd}');
            out.push(c);
        } else if chars[i] == '\n' {
            i += 1;
        } else if chars[i] == '"' || chars[i] == '\'' {
            out.push('\\');
            out.push(chars[i]);
            i += 1;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn skip_quoted(b: &[u8], mut i: usize) -> usize {
    let q = b[i];
    i += 1;
    while i < b.len() && b[i] != q {
        if b[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    (i + 1).min(b.len())
}

fn strip_css_comments(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'"' || b[i] == b'\'' {
            let end = skip_quoted(b, i);
            out.push_str(&s[i..end]);
            i = end;
            continue;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            out.push(' ');
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn strip_imports(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("@import") {
        let start = pos + rel;
        out.push_str(&s[pos..start]);
        let b = s.as_bytes();
        let mut i = start;
        while i < b.len() && b[i] != b';' && b[i] != b'}' {
            if b[i] == b'"' || b[i] == b'\'' {
                i = skip_quoted(b, i);
            } else {
                i += 1;
            }
        }
        pos = if i < b.len() && b[i] == b';' {
            i + 1
        } else {
            i
        };
    }
    out.push_str(&s[pos..]);
    out
}

fn find_func_call(lower: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let b = lower.as_bytes();
    let mut pos = from;
    while let Some(rel) = lower[pos..].find(name) {
        let start = pos + rel;
        let prev_ident = start > 0
            && (b[start - 1].is_ascii_alphanumeric()
                || b[start - 1] == b'-'
                || b[start - 1] == b'_');
        let mut after = start + name.len();
        while after < b.len() && b[after].is_ascii_whitespace() {
            after += 1;
        }
        if !prev_ident && after < b.len() && b[after] == b'(' {
            return Some((start, after + 1));
        }
        pos = start + name.len();
    }
    None
}

fn consume_balanced_parens(b: &[u8], mut i: usize) -> usize {
    let mut depth = 1usize;
    while i < b.len() && depth > 0 {
        match b[i] {
            b'"' | b'\'' => i = skip_quoted(b, i),
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    i
}

fn scrub_css_func(s: &str, name: &str, allow_safe_image_url: bool) -> String {
    let lower = s.to_ascii_lowercase();
    let b = s.as_bytes();
    let mut out = String::new();
    let mut pos = 0;
    while let Some((start, arg_start)) = find_func_call(&lower, pos, name) {
        out.push_str(&s[pos..start]);
        let end = consume_balanced_parens(b, arg_start);
        let raw_inner = s[arg_start..end.saturating_sub(1).max(arg_start)].trim();
        let unquoted = if (raw_inner.starts_with('"')
            && raw_inner.ends_with('"')
            && raw_inner.len() >= 2)
            || (raw_inner.starts_with('\'') && raw_inner.ends_with('\'') && raw_inner.len() >= 2)
        {
            &raw_inner[1..raw_inner.len() - 1]
        } else {
            raw_inner
        };
        if allow_safe_image_url && url_allowed(unquoted, UrlContext::Image) {
            out.push_str("url(");
            out.push_str(unquoted.trim());
            out.push(')');
        } else {
            out.push_str("none");
        }
        pos = end;
    }
    out.push_str(&s[pos..]);
    out
}

fn scrub_css_keyword_followed_by(s: &str, kw: &str, delim: u8) -> String {
    let lower = s.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut out = String::new();
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find(kw) {
        let start = pos + rel;
        let mut after = start + kw.len();
        while after < b.len() && b[after].is_ascii_whitespace() {
            after += 1;
        }
        if after < b.len() && b[after] == delim {
            out.push_str(&s[pos..start]);
            out.push(' ');
            pos = after + 1;
        } else {
            out.push_str(&s[pos..start + kw.len()]);
            pos = start + kw.len();
        }
    }
    out.push_str(&s[pos..]);
    out
}

fn scrub_once(s: &str) -> String {
    let mut out = strip_css_comments(s);
    out = css_unescape(&out);
    out = strip_imports(&out);
    out = scrub_css_func(&out, "url", true);
    out = scrub_css_func(&out, "src", true);
    out = scrub_css_func(&out, "image-set", false);
    out = scrub_css_func(&out, "-webkit-image-set", false);
    out = scrub_css_keyword_followed_by(&out, "expression", b'(');
    out = scrub_css_keyword_followed_by(&out, "behavior", b':');
    out = scrub_css_keyword_followed_by(&out, "-moz-binding", b':');
    for bad in ["javascript:", "vbscript:"] {
        while let Some(i) = out.to_ascii_lowercase().find(bad) {
            out.replace_range(i..i + bad.len(), " ");
        }
    }
    out
}

fn sanitize_css(s: &str) -> String {
    let mut cur = s.replace("\r\n", "\n").replace(['\r', '\x0c', '\0'], "\n");
    for _ in 0..8 {
        let next = scrub_once(&cur);
        if next == cur {
            break;
        }
        cur = next;
    }
    // Prevent raw-text `</style>` or HTML comment breakout while keeping `>` child combinators.
    cur.replace('<', "\\3c ")
}

fn sanitize_nodes(nodes: Vec<Node>) -> Vec<Node> {
    nodes.into_iter().filter_map(sanitize_node).collect()
}

fn sanitize_node(node: Node) -> Option<Node> {
    let Node::El(mut el) = node else {
        return Some(node);
    };
    let tag = el.tag.to_ascii_lowercase();
    if DROP_TAGS.contains(&tag.as_str()) {
        return None;
    }
    if tag == "meta" {
        let has_charset_only = el
            .attrs
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("charset"))
            && !el
                .attrs
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("http-equiv"));
        if !has_charset_only {
            return None;
        }
    }
    let attrs = std::mem::take(&mut el.attrs);
    for (name, value) in attrs {
        let lname = name.to_ascii_lowercase();
        if let Some(clean) = sanitize_attr(&tag, &lname, &value) {
            el.attrs.push((lname, clean));
        }
    }
    if tag == "style" {
        for child in el.children.iter_mut() {
            if let Node::Text(t) = child {
                *t = sanitize_css(t);
            }
        }
    }
    el.children = sanitize_nodes(std::mem::take(&mut el.children));
    Some(Node::El(el))
}

fn is_valid_attr_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':'))
}

fn is_valid_css_prop(prop: &str) -> bool {
    !prop.is_empty()
        && prop.len() <= 64
        && prop
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn sanitize_attr(tag: &str, lname: &str, value: &str) -> Option<String> {
    if lname.starts_with("on")
        || lname
            .rsplit_once(':')
            .is_some_and(|(_, local)| local.starts_with("on"))
        || STRIP_ATTRS.contains(&lname)
    {
        return None;
    }
    if lname == "style" {
        return Some(sanitize_css(value));
    }
    if lname == "srcset" {
        return srcset_allowed(value).then(|| value.trim().to_string());
    }
    if lname == "href" || lname == "xlink:href" {
        let ctx = match tag {
            "use" => UrlContext::FragmentOnly,
            "image" | "feimage" => UrlContext::Image,
            _ => UrlContext::Link,
        };
        return url_allowed(value, ctx).then(|| value.trim().to_string());
    }
    if lname == "src" || lname == "poster" {
        return url_allowed(value, UrlContext::Image).then(|| value.trim().to_string());
    }
    Some(value.to_string())
}

fn esc_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
}

fn esc_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
}

fn serialize(nodes: &[Node], raw: bool, out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) => {
                if raw {
                    out.push_str(t);
                } else {
                    esc_text(t, out);
                }
            }
            Node::El(el) => {
                out.push('<');
                out.push_str(&el.tag);
                let mut attrs: Vec<&(String, String)> = el.attrs.iter().collect();
                attrs.sort_by(|a, b| a.0.cmp(&b.0));
                for (k, v) in attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    esc_attr(v, out);
                    out.push('"');
                }
                out.push('>');
                let lower = el.tag.to_ascii_lowercase();
                if VOID.contains(&lower.as_str()) {
                    continue;
                }
                serialize(&el.children, RAW_TEXT.contains(&lower.as_str()), out);
                out.push_str("</");
                out.push_str(&el.tag);
                out.push('>');
            }
        }
    }
}

fn to_html(nodes: &[Node]) -> String {
    let mut out = String::new();
    serialize(nodes, false, &mut out);
    out
}

fn needs_id(el: &El) -> bool {
    !NO_ID_TAGS.contains(&el.tag.to_ascii_lowercase().as_str())
}

fn collect_ids(nodes: &[Node], out: &mut Vec<String>) {
    for n in nodes {
        if let Node::El(el) = n {
            if let Some(id) = el.attr(ID_ATTR)
                && !id.is_empty()
            {
                out.push(id.to_string());
            }
            collect_ids(&el.children, out);
        }
    }
}

pub fn element_ids(html: &str) -> Vec<String> {
    let Ok(parsed) = parse_roots(html) else {
        return Vec::new();
    };
    let roots = sanitize_nodes(parsed);
    let mut ids = Vec::new();
    collect_ids(&roots, &mut ids);
    ids
}

type Key1 = (String, Vec<usize>, String);
type Key2 = (String, String, String);

struct Info {
    key1: Key1,
    key2: Key2,
}

fn attr_sig(el: &El) -> String {
    let mut attrs: Vec<String> = el
        .attrs
        .iter()
        .filter(|(k, _)| k != ID_ATTR)
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    attrs.sort();
    attrs.join("\u{1}")
}

fn info_of(el: &El, path: &[usize]) -> Info {
    let text = el.direct_text();
    Info {
        key1: (el.tag.clone(), path.to_vec(), text.clone()),
        key2: (el.tag.clone(), text, attr_sig(el)),
    }
}

fn walk_old(nodes: &[Node], path: &mut Vec<usize>, out: &mut Vec<(String, Info)>) {
    let mut idx = 0;
    for n in nodes {
        if let Node::El(el) = n {
            path.push(idx);
            idx += 1;
            if let Some(id) = el.attr(ID_ATTR).filter(|s| !s.is_empty()) {
                out.push((id.to_string(), info_of(el, path)));
            }
            walk_old(&el.children, path, out);
            path.pop();
        }
    }
}

struct Pending {
    info: Info,
}

struct Resolve<'a> {
    old_ids: &'a HashSet<String>,
    taken_new: HashSet<String>,
    used_old: HashSet<String>,
    kept: usize,
    pending: Vec<Pending>,
}

fn pass_a(nodes: &mut [Node], path: &mut Vec<usize>, r: &mut Resolve<'_>) {
    let mut idx = 0;
    for n in nodes.iter_mut() {
        if let Node::El(el) = n {
            path.push(idx);
            idx += 1;
            if needs_id(el) {
                let carried = el
                    .attr(ID_ATTR)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                match carried {
                    Some(id) if r.taken_new.insert(id.clone()) => {
                        if r.old_ids.contains(&id) {
                            r.used_old.insert(id);
                        }
                        r.kept += 1;
                    }
                    _ => {
                        el.remove_attr(ID_ATTR);
                        r.pending.push(Pending {
                            info: info_of(el, path),
                        });
                    }
                }
            } else {
                el.remove_attr(ID_ATTR);
            }
            pass_a(&mut el.children, path, r);
            path.pop();
        }
    }
}

fn pass_c(
    nodes: &mut [Node],
    assigned: &mut VecDeque<Option<String>>,
    ids: &mut IdGen,
    new: &mut usize,
) {
    for n in nodes.iter_mut() {
        if let Node::El(el) = n {
            if needs_id(el) && el.attr(ID_ATTR).is_none() {
                let id = match assigned.pop_front().flatten() {
                    Some(id) => id,
                    None => {
                        *new += 1;
                        ids.next()
                    }
                };
                ids.reserve(&id);
                el.set_attr(ID_ATTR, id);
            }
            pass_c(&mut el.children, assigned, ids, new);
        }
    }
}

fn assign_ids(roots: &mut [Node], prev: Option<&[Node]>, ids: &mut IdGen) -> (usize, usize) {
    let mut old = Vec::new();
    if let Some(prev) = prev {
        walk_old(prev, &mut Vec::new(), &mut old);
    }
    let mut present = Vec::new();
    collect_ids(roots, &mut present);
    for id in present.iter().chain(old.iter().map(|(id, _)| id)) {
        ids.reserve(id);
    }
    let old_ids: HashSet<String> = old.iter().map(|(id, _)| id.clone()).collect();
    let mut r = Resolve {
        old_ids: &old_ids,
        taken_new: HashSet::new(),
        used_old: HashSet::new(),
        kept: 0,
        pending: Vec::new(),
    };
    pass_a(roots, &mut Vec::new(), &mut r);

    let mut free: Vec<(String, Info)> = old
        .into_iter()
        .filter(|(id, _)| !r.used_old.contains(id))
        .collect();
    let mut assigned: Vec<Option<String>> = vec![None; r.pending.len()];

    let mut by1: HashMap<Key1, VecDeque<String>> = HashMap::new();
    for (id, info) in &free {
        by1.entry(info.key1.clone())
            .or_default()
            .push_back(id.clone());
    }
    let mut claimed: HashSet<String> = HashSet::new();
    for (i, p) in r.pending.iter().enumerate() {
        if let Some(q) = by1.get_mut(&p.info.key1) {
            while let Some(id) = q.pop_front() {
                if claimed.insert(id.clone()) {
                    assigned[i] = Some(id);
                    break;
                }
            }
        }
    }
    free.retain(|(id, _)| !claimed.contains(id));
    let mut by2: HashMap<Key2, VecDeque<String>> = HashMap::new();
    for (id, info) in &free {
        by2.entry(info.key2.clone())
            .or_default()
            .push_back(id.clone());
    }
    for (i, p) in r.pending.iter().enumerate() {
        if assigned[i].is_some() {
            continue;
        }
        if let Some(q) = by2.get_mut(&p.info.key2)
            && let Some(id) = q.pop_front()
        {
            assigned[i] = Some(id);
        }
    }
    let taken: HashSet<String> = assigned.iter().flatten().cloned().collect();
    free.retain(|(id, _)| !taken.contains(id));
    let mut by3: HashMap<(String, Vec<usize>), VecDeque<String>> = HashMap::new();
    for (id, info) in &free {
        by3.entry((info.key1.0.clone(), info.key1.1.clone()))
            .or_default()
            .push_back(id.clone());
    }
    for (i, p) in r.pending.iter().enumerate() {
        if assigned[i].is_some() {
            continue;
        }
        if let Some(q) = by3.get_mut(&(p.info.key1.0.clone(), p.info.key1.1.clone()))
            && let Some(id) = q.pop_front()
        {
            assigned[i] = Some(id);
        }
    }
    let matched = assigned.iter().filter(|a| a.is_some()).count();
    let mut new = 0;
    let mut queue: VecDeque<Option<String>> = assigned.into();
    pass_c(roots, &mut queue, ids, &mut new);
    (r.kept + matched, new)
}

pub fn normalize(html: &str, prev: Option<&str>, ids: &mut IdGen) -> Result<Normalized, HtmlError> {
    let mut roots = sanitize_nodes(parse_roots(html)?);
    let prev_roots = match prev {
        Some(p) => Some(sanitize_nodes(parse_roots(p)?)),
        None => None,
    };
    let (kept, new) = assign_ids(&mut roots, prev_roots.as_deref(), ids);
    let out = to_html(&roots);
    check_raw_html(&out)?;
    Ok(Normalized {
        html: out,
        ids_kept: kept,
        ids_new: new,
    })
}

fn find_path(nodes: &[Node], id: &str) -> Option<Vec<usize>> {
    for (i, n) in nodes.iter().enumerate() {
        if let Node::El(el) = n {
            if el.attr(ID_ATTR) == Some(id) {
                return Some(vec![i]);
            }
            if let Some(mut rest) = find_path(&el.children, id) {
                rest.insert(0, i);
                return Some(rest);
            }
        }
    }
    None
}

fn list_at_mut<'a>(roots: &'a mut Vec<Node>, path: &[usize]) -> &'a mut Vec<Node> {
    let mut list = roots;
    for &i in path {
        list = match &mut list[i] {
            Node::El(el) => &mut el.children,
            Node::Text(_) => unreachable!("path points through text"),
        };
    }
    list
}

fn el_at<'a>(roots: &'a [Node], path: &[usize]) -> Option<&'a El> {
    let mut list = roots;
    let mut cur = None;
    for &i in path {
        match list.get(i)? {
            Node::El(el) => {
                cur = Some(el);
                list = &el.children;
            }
            Node::Text(_) => return None,
        }
    }
    cur
}

fn parent_path(roots: &[Node], parent_id: &str) -> Result<Vec<usize>, HtmlError> {
    if parent_id == ROOT_ID || parent_id.is_empty() {
        return Ok(Vec::new());
    }
    let path = find_path(roots, parent_id).ok_or_else(|| HtmlError::NotFound(parent_id.into()))?;
    let el = el_at(roots, &path)
        .ok_or_else(|| HtmlError::BadParent(format!("parent `{parent_id}` is not an element")))?;
    if VOID.contains(&el.tag.to_ascii_lowercase().as_str()) {
        return Err(HtmlError::BadParent(format!(
            "cannot insert children into void element `<{}>` (`{parent_id}`)",
            el.tag
        )));
    }
    Ok(path)
}

fn id_of_path(roots: &[Node], parent: &[usize]) -> String {
    el_at(roots, parent)
        .and_then(|e| e.attr(ID_ATTR))
        .unwrap_or(ROOT_ID)
        .to_string()
}

fn element_index(list: &[Node], node_idx: usize) -> usize {
    list[..node_idx]
        .iter()
        .filter(|n| matches!(n, Node::El(_)))
        .count()
}

fn node_position(list: &[Node], index: usize) -> usize {
    list.iter()
        .enumerate()
        .filter(|(_, n)| matches!(n, Node::El(_)))
        .nth(index)
        .map(|(i, _)| i)
        .unwrap_or(list.len())
}

fn detach(list: &mut Vec<Node>, at: usize) -> Vec<Node> {
    let mut end = at + 1;
    while end < list.len() && matches!(list[end], Node::Text(_)) {
        end += 1;
    }
    list.drain(at..end).collect()
}

fn parse_doc(html: &str, ids: &mut IdGen) -> Result<Vec<Node>, HtmlError> {
    let mut roots = sanitize_nodes(parse_roots(html)?);
    assign_ids(&mut roots, None, ids);
    Ok(roots)
}

fn prepare_insert(html: &str, doc: &[Node], ids: &mut IdGen) -> Result<Vec<Node>, HtmlError> {
    let mut nodes = sanitize_nodes(parse_roots(html)?);
    let mut existing = Vec::new();
    collect_ids(doc, &mut existing);
    for id in &existing {
        ids.reserve(id);
    }
    let mut seen: HashSet<String> = existing.into_iter().collect();
    fn dedupe(nodes: &mut [Node], seen: &mut HashSet<String>) {
        for n in nodes.iter_mut() {
            if let Node::El(el) = n {
                if let Some(id) = el.attr(ID_ATTR).map(str::to_string)
                    && (id.is_empty() || !seen.insert(id))
                {
                    el.remove_attr(ID_ATTR);
                }
                dedupe(&mut el.children, seen);
            }
        }
    }
    dedupe(&mut nodes, &mut seen);
    let mut inserted = Vec::new();
    collect_ids(&nodes, &mut inserted);
    for id in &inserted {
        ids.reserve(id);
    }
    assign_ids(&mut nodes, None, ids);
    let roots = nodes.iter().filter(|n| matches!(n, Node::El(_))).count();
    let first_ok = matches!(nodes.first(), Some(Node::El(e)) if e.attr(ID_ATTR).is_some());
    if roots != 1 || !first_ok {
        return Err(HtmlError::Parse(
            "insert html must have exactly one root element".into(),
        ));
    }
    Ok(nodes)
}

fn merge_style(prev: &str, changes: &[(String, Option<String>)]) -> String {
    let mut decls: Vec<(String, String)> = Vec::new();
    let b = prev.as_bytes();
    let mut start = 0;
    let mut depth = 0i32;
    let mut i = 0;
    let mut parts = Vec::new();
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => {
                i = skip_quoted(b, i);
                continue;
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            b';' if depth <= 0 => {
                parts.push(&prev[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&prev[start..]);
    for part in parts {
        if let Some((k, v)) = part.split_once(':') {
            let k = k.trim();
            if !k.is_empty() {
                decls.push((k.to_string(), v.trim().to_string()));
            }
        }
    }
    for (prop, value) in changes {
        let prop = prop.trim();
        if !is_valid_css_prop(prop) {
            continue;
        }
        match value {
            Some(v) => {
                // Prevent semicolon/brace injection out of a single style property value.
                let v = v.trim().replace([';', '{', '}'], " ");
                match decls.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(prop)) {
                    Some(slot) => slot.1 = v,
                    None => decls.push((prop.to_string(), v)),
                }
            }
            None => decls.retain(|(k, _)| !k.eq_ignore_ascii_case(prop)),
        }
    }
    decls
        .into_iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn apply_dom_op(html: &str, op: &DomOp, ids: &mut IdGen) -> Result<(String, DomOp), HtmlError> {
    let (out, _concrete, inverse) = apply_dom_op_with_concrete(html, op, ids)?;
    Ok((out, inverse))
}

/// Apply a DOM op and return `(resulting_html, concrete_op, inverse_op)`.
/// `concrete_op` carries normalized, ID-assigned HTML for `Insert` so log replay is deterministic.
pub fn apply_dom_op_with_concrete(
    html: &str,
    op: &DomOp,
    ids: &mut IdGen,
) -> Result<(String, DomOp, DomOp), HtmlError> {
    let mut roots = parse_doc(html, ids)?;
    let (concrete, inverse) = match op {
        DomOp::Patch {
            el_id,
            attrs,
            style,
            text,
        } => {
            let path =
                find_path(&roots, el_id).ok_or_else(|| HtmlError::NotFound(el_id.clone()))?;
            let (parent, last) = path.split_at(path.len() - 1);
            let list = list_at_mut(&mut roots, parent);
            let Node::El(el) = &mut list[last[0]] else {
                return Err(HtmlError::NotFound(el_id.clone()));
            };
            let tag_lower = el.tag.to_ascii_lowercase();
            if text.is_some() && VOID.contains(&tag_lower.as_str()) {
                return Err(HtmlError::BadParent(format!(
                    "cannot set text on void element `<{}>` (`{el_id}`)",
                    el.tag
                )));
            }
            for (k, _) in attrs {
                if !is_valid_attr_name(k) {
                    return Err(HtmlError::Parse(format!("invalid attribute name: `{k}`")));
                }
            }
            for (prop, _) in style {
                let p = prop.trim();
                if !is_valid_css_prop(p) {
                    return Err(HtmlError::Parse(format!(
                        "invalid CSS property name: `{prop}`"
                    )));
                }
            }
            let mut keys: Vec<String> = Vec::new();
            for (k, _) in attrs {
                let lk = k.to_ascii_lowercase();
                if lk != ID_ATTR && !keys.contains(&lk) {
                    keys.push(lk);
                }
            }
            if !style.is_empty() && !keys.iter().any(|k| k == "style") {
                keys.push("style".into());
            }
            let snapshot: Vec<(String, Option<String>)> = keys
                .iter()
                .map(|k| (k.clone(), el.attr(k).map(str::to_string)))
                .collect();
            for (k, v) in attrs {
                let lk = k.to_ascii_lowercase();
                if lk == ID_ATTR {
                    continue;
                }
                match v {
                    Some(v) => match sanitize_attr(&tag_lower, &lk, v) {
                        Some(clean) => el.set_attr(&lk, clean),
                        None => el.remove_attr(&lk),
                    },
                    None => el.remove_attr(&lk),
                }
            }
            if !style.is_empty() {
                let merged = merge_style(el.attr("style").unwrap_or(""), style);
                let merged = sanitize_css(&merged);
                if merged.is_empty() {
                    el.remove_attr("style");
                } else {
                    el.set_attr("style", merged);
                }
            }
            let prev_text = match text {
                Some(new_text) => {
                    let lead = el
                        .children
                        .iter()
                        .take_while(|c| matches!(c, Node::Text(_)))
                        .count();
                    let prev: String = el
                        .children
                        .drain(..lead)
                        .filter_map(|c| match c {
                            Node::Text(t) => Some(t),
                            Node::El(_) => None,
                        })
                        .collect();
                    if !new_text.is_empty() {
                        el.children.insert(0, Node::Text(new_text.clone()));
                    }
                    Some(prev)
                }
                None => None,
            };
            let concrete_attrs: Vec<(String, Option<String>)> = keys
                .iter()
                .map(|k| (k.clone(), el.attr(k).map(str::to_string)))
                .collect();
            let concrete = DomOp::Patch {
                el_id: el_id.clone(),
                attrs: concrete_attrs,
                style: Vec::new(),
                text: text.clone(),
            };
            let inverse = DomOp::Patch {
                el_id: el_id.clone(),
                attrs: snapshot,
                style: Vec::new(),
                text: prev_text,
            };
            (concrete, inverse)
        }
        DomOp::Insert {
            parent_id,
            index,
            html: frag,
        } => {
            let nodes = prepare_insert(frag, &roots, ids)?;
            let concrete_html = to_html(&nodes);
            let new_id = match &nodes[0] {
                Node::El(e) => e.attr(ID_ATTR).unwrap_or_default().to_string(),
                Node::Text(_) => String::new(),
            };
            let ppath = parent_path(&roots, parent_id)?;
            let list = list_at_mut(&mut roots, &ppath);
            let pos = node_position(list, *index);
            list.splice(pos..pos, nodes);
            let concrete = DomOp::Insert {
                parent_id: parent_id.clone(),
                index: *index,
                html: concrete_html,
            };
            (concrete, DomOp::Remove { el_id: new_id })
        }
        DomOp::Remove { el_id } => {
            let path =
                find_path(&roots, el_id).ok_or_else(|| HtmlError::NotFound(el_id.clone()))?;
            let (parent, last) = path.split_at(path.len() - 1);
            let parent_id = id_of_path(&roots, parent);
            let list = list_at_mut(&mut roots, parent);
            let index = element_index(list, last[0]);
            let removed = detach(list, last[0]);
            (
                DomOp::Remove {
                    el_id: el_id.clone(),
                },
                DomOp::Insert {
                    parent_id,
                    index,
                    html: to_html(&removed),
                },
            )
        }
        DomOp::Move {
            el_id,
            parent_id,
            index,
        } => {
            let path =
                find_path(&roots, el_id).ok_or_else(|| HtmlError::NotFound(el_id.clone()))?;
            let target = parent_path(&roots, parent_id)?;
            if target.len() >= path.len() && target[..path.len()] == path[..] {
                return Err(HtmlError::BadParent(format!(
                    "cannot move element `{el_id}` into itself or a descendant"
                )));
            }
            let (parent, last) = path.split_at(path.len() - 1);
            let old_parent_id = id_of_path(&roots, parent);
            let list = list_at_mut(&mut roots, parent);
            let old_index = element_index(list, last[0]);
            let removed = detach(list, last[0]);
            let mut adjusted = target.clone();
            if target.len() > parent.len()
                && target[..parent.len()] == *parent
                && target[parent.len()] > last[0]
            {
                adjusted[parent.len()] -= removed.len();
            }
            let tlist = list_at_mut(&mut roots, &adjusted);
            let pos = node_position(tlist, *index);
            tlist.splice(pos..pos, removed);
            (
                DomOp::Move {
                    el_id: el_id.clone(),
                    parent_id: parent_id.clone(),
                    index: *index,
                },
                DomOp::Move {
                    el_id: el_id.clone(),
                    parent_id: old_parent_id,
                    index: old_index,
                },
            )
        }
    };
    let out = to_html(&roots);
    check_raw_html(&out)?;
    let mut scratch = IdGen::new(&[]);
    if to_html(&parse_doc(&out, &mut scratch)?) != out {
        return Err(HtmlError::BadParent(
            "operation produced invalid HTML nesting that would not round-trip".into(),
        ));
    }
    Ok((out, concrete, inverse))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(html: &str) -> String {
        normalize(html, None, &mut IdGen::new(&[])).unwrap().html
    }

    #[test]
    fn sanitiser_strips_vectors() {
        let dirty = r##"<div onclick="x()" style="background:url(http://e.com/a.png);color:red"><script>alert(1)</script>
<iframe src="x"></iframe><object data="x"></object><embed src="x">
<link rel="stylesheet" href="http://e.com/a.css"><meta http-equiv="refresh" content="0;url=http://e.com">
<form action="http://e.com"><input formaction="http://e.com"></form>
<a href="javascript:alert(1)">a</a><a href=" JaVa&#10;script:1">b</a><a href="vbscript:x">c</a>
<a href="https://e.com">d</a><a href="//e.com/x">e</a><a href="#frag">f</a>
<img src="http://e.com/i.png"><img src="data:image/png;base64,AAAA"><img src="/canvas/assets/a.png">
<img srcset="http://e.com/a.png 1x, /canvas/assets/b.png 2x"><video poster="https://e.com/p.png"></video>
<p onmouseover="x">hi</p></div>
<style>@import url(http://e.com/x.css); @import "y.css"; .a{background:url("https://e.com/b.png")} .b{background:url(data:image/png;base64,AA)} .c{background:u\72l(http://e.com/c)}</style>"##;
        let out = norm(dirty);
        let lower = out.to_ascii_lowercase();
        for bad in [
            "<script",
            "<iframe",
            "<object",
            "<embed",
            "<link",
            "refresh",
            "action=",
            "onclick",
            "onmouseover",
            "javascript:",
            "vbscript:",
            "http://",
            "https://",
            "//e.com",
            "@import",
            "alert(",
            "y.css",
            "srcset",
            "poster",
        ] {
            assert!(!lower.contains(bad), "{bad} survived: {out}");
        }
        for good in [
            "href=\"#frag\"",
            "data:image/png;base64,AAAA",
            "/canvas/assets/a.png",
            "color: red",
            "url(data:image/png;base64,AA)",
        ] {
            assert!(
                out.contains(good) || out.contains(&good.replace("color: red", "color:red")),
                "{good} lost: {out}"
            );
        }
    }

    #[test]
    fn sanitiser_blocks_hostile_bypasses_and_dos() {
        // 1. data: restricted to raster images only, never in href, never SVG/HTML/JS.
        let data_attacks = r##"
            <a href="data:image/png;base64,AAAA">a</a>
            <a href="data:text/html,<script>alert(1)</script>">b</a>
            <img src="data:image/svg+xml,<svg onload=alert(1)>">
            <img src="data:image/svg+xml;base64,PHN2ZyBvbmxvYWQ9YWxlcnQoMSk+">
            <img src="data:text/javascript,alert(1)">
            <img src="data:;base64,PHNjcmlwdD4=">
            <img src="/canvas/assets/%2e%2e/secret.png">
            <img src="/canvas/assets/..hidden/a.png">
            <img srcset="data:image/png;base64,AAAA,https://evil.com/x.png">
            <img srcset="/canvas/assets/a.png,https://evil.com/x.png">
            <img srcset="data:image/svg+xml;base64,AAAA 1x">
            <img srcset="data:image/png;base64,AAAA 1x, /canvas/assets/ok.png 2x">
        "##;
        let out = norm(data_attacks);
        let lower = out.to_ascii_lowercase();
        for bad in [
            "image/svg+xml",
            "text/html",
            "text/javascript",
            "data:;base64",
            "%2e%2e",
            "..hidden",
            "evil.com",
            "href=\"data:",
        ] {
            assert!(!lower.contains(bad), "{bad} survived: {out}");
        }
        assert!(out.contains("srcset=\"data:image/png;base64,AAAA 1x, /canvas/assets/ok.png 2x\""));

        // 2. SVG foreignObject, external <use>, SMIL <set>/<animate>, <link>, <meta>.
        let svg_attacks = r##"
            <svg>
                <foreignObject><div>xss</div></foreignObject>
                <use href="https://evil.com/s.svg#x"></use>
                <use xlink:href="/canvas/assets/other.svg#x"></use>
                <use href="data:image/svg+xml;base64,AA#x"></use>
                <use href="#local-icon"></use>
                <set attributeName="onmouseover" to="alert(1)"></set>
                <animate attributeName="href" values="javascript:alert(1)"></animate>
                <image href="data:image/svg+xml;base64,AA"></image>
                <image href="data:image/png;base64,AA"></image>
            </svg>
            <link rel="stylesheet-data" href="data:text/css,@import url(http://evil.com)">
            <meta http-equiv="content-security-policy" content="default-src *">
            <template><script>alert(1)</script></template>
            <noscript><img src="http://evil.com/x.png"></noscript>
        "##;
        let out = norm(svg_attacks);
        let lower = out.to_ascii_lowercase();
        for bad in [
            "foreignobject",
            "evil.com",
            "other.svg",
            "<set",
            "<animate",
            "<link",
            "content-security-policy",
            "<template",
            "<noscript",
            "image/svg+xml",
        ] {
            assert!(!lower.contains(bad), "{bad} survived: {out}");
        }
        assert!(out.contains("href=\"#local-icon\""));
        assert!(out.contains("href=\"data:image/png;base64,AA\""));

        // 3. CSS escape </style> breakout, comments, image-set, expression/behavior.
        let css_attacks = concat!(
            r#"<style>"#,
            r#".a { content: "\3c /style\3e <script>alert(1)</script>"; background: url(http://evil.com); }"#,
            r#" .d { background: image-set("https://evil.com/a.png" 1x); }"#,
            r#" .e { behavior: url(http://evil.com/x.htc); -moz-binding: url(http://evil.com/x.xml); }"#,
            r#" .f { /* comment */ background: /*x*/ url(https://evil.com/y.png); }"#,
            r#" div > p { color: green; }"#,
            r#"</style>"#,
        );
        let out = norm(css_attacks);
        let lower = out.to_ascii_lowercase();
        for bad in [
            "<script",
            "evil.com",
            "@import",
            "expression(",
            "javascript:",
            "vbscript:",
            "behavior:",
            "-moz-binding:",
            "image-set(",
        ] {
            assert!(!lower.contains(bad), "{bad} survived in CSS: {out}");
        }
        // Only the single closing </style> tag of the style element itself may appear.
        assert_eq!(
            lower.matches("</style>").count(),
            1,
            "style breakout: {out}"
        );
        assert!(out.contains("div > p { color: green; }"));
        // Idempotent after escaping `<`.
        assert_eq!(norm(&out), out);

        // 3b. Split-keyword tricks must NEVER reassemble into url(, @import, or expression(.
        let split_css = concat!(
            r#"<style>"#,
            r#".b { background: urjavascript:l(http://evil.com/a.png); }"#,
            r#" @imjavascript:port "http://evil.com/x.css";"#,
            r#" .c { width: expresvbscript:sion(alert(1)); }"#,
            r#"</style>"#,
        );
        let split_out = norm(split_css).to_ascii_lowercase();
        for bad in ["url(", "@import", "expression(", "javascript:", "vbscript:"] {
            assert!(
                !split_out.contains(bad),
                "{bad} reassembled in CSS: {split_out}"
            );
        }

        // 4. Huge input & deep nesting DoS rejected cleanly (no stack overflow).
        let huge = format!("<div>{}</div>", "a".repeat(MAX_HTML_BYTES + 1));
        assert!(matches!(
            normalize(&huge, None, &mut IdGen::new(&[])),
            Err(HtmlError::TooLarge(_))
        ));
        let deep = format!(
            "{}{}",
            "<div>".repeat(MAX_DOM_DEPTH + 30),
            "</div>".repeat(MAX_DOM_DEPTH + 30)
        );
        assert!(matches!(
            normalize(&deep, None, &mut IdGen::new(&[])),
            Err(HtmlError::TooDeep(_))
        ));

        // 5. DomOp::Patch rejects invalid attr names and cannot spoof data-cv-id via uppercase.
        let mut g = IdGen::new(&[]);
        let doc = normalize("<div>hi</div>", None, &mut g).unwrap().html;
        let id = element_ids(&doc)[0].clone();
        assert!(matches!(
            apply_dom_op(
                &doc,
                &DomOp::Patch {
                    el_id: id.clone(),
                    attrs: vec![("x\" onclick=\"alert(1)".into(), Some("1".into()))],
                    style: vec![],
                    text: None,
                },
                &mut g,
            ),
            Err(HtmlError::Parse(_))
        ));
        let (patched, _) = apply_dom_op(
            &doc,
            &DomOp::Patch {
                el_id: id.clone(),
                attrs: vec![("DATA-CV-ID".into(), Some("spoofed".into()))],
                style: vec![],
                text: None,
            },
            &mut g,
        )
        .unwrap();
        assert_eq!(element_ids(&patched), vec![id]);
    }

    #[test]
    fn normalize_is_idempotent_and_assigns_ids() {
        let mut g = IdGen::new(&[]);
        let n = normalize("<div><p>a</p><p>b</p></div>", None, &mut g).unwrap();
        assert_eq!(n.ids_new, 3);
        assert_eq!(element_ids(&n.html).len(), 3);
        let again = normalize(&n.html, Some(&n.html), &mut g).unwrap();
        assert_eq!(again.html, n.html);
        assert_eq!(again.ids_kept, 3);
        assert_eq!(again.ids_new, 0);
        let doc =
            norm("<html><head><style>p{color:red}</style></head><body><p>x</p></body></html>");
        assert!(doc.starts_with("<style>p{color:red}</style><p data-cv-id="));
        assert_eq!(norm(&doc), doc);
    }

    fn id_of(html: &str, text: &str) -> String {
        let roots = parse_roots(html).unwrap();
        fn walk(nodes: &[Node], text: &str) -> Option<String> {
            for n in nodes {
                if let Node::El(e) = n {
                    if e.direct_text() == text {
                        return e.attr(ID_ATTR).map(str::to_string);
                    }
                    if let Some(r) = walk(&e.children, text) {
                        return Some(r);
                    }
                }
            }
            None
        }
        walk(&roots, text).unwrap()
    }

    #[test]
    fn ids_stable_across_replacement() {
        let mut g = IdGen::new(&[]);
        let base = normalize(
            "<div><h1>Title</h1><p>one</p><p>two</p><ul><li>a</li><li>b</li></ul></div>",
            None,
            &mut g,
        )
        .unwrap()
        .html;
        let (h1, one, two, a, b) = (
            id_of(&base, "Title"),
            id_of(&base, "one"),
            id_of(&base, "two"),
            id_of(&base, "a"),
            id_of(&base, "b"),
        );

        let edited = normalize(
            "<div><h1>Title</h1><p>one changed</p><p>two</p><ul><li>a</li><li>b</li></ul></div>",
            Some(&base),
            &mut g,
        )
        .unwrap();
        assert_eq!(id_of(&edited.html, "Title"), h1);
        assert_eq!(id_of(&edited.html, "two"), two);
        assert_eq!(id_of(&edited.html, "one changed"), one);
        assert_eq!(edited.ids_new, 0);

        let inserted = normalize(
            "<div><h1>Title</h1><section>new</section><p>one</p><p>two</p><ul><li>a</li><li>b</li></ul></div>",
            Some(&base),
            &mut g,
        )
        .unwrap();
        assert_eq!(inserted.ids_new, 1);
        assert_eq!(id_of(&inserted.html, "one"), one);
        assert_eq!(id_of(&inserted.html, "two"), two);
        assert_eq!(id_of(&inserted.html, "b"), b);

        let reordered = normalize(
            "<div><h1>Title</h1><p>two</p><p>one</p><ul><li>b</li><li>a</li></ul></div>",
            Some(&base),
            &mut g,
        )
        .unwrap();
        assert_eq!(reordered.ids_new, 0);
        assert_eq!(id_of(&reordered.html, "one"), one);
        assert_eq!(id_of(&reordered.html, "two"), two);
        assert_eq!(id_of(&reordered.html, "a"), a);
        assert_eq!(id_of(&reordered.html, "b"), b);

        let mut all = element_ids(&inserted.html);
        all.sort();
        all.dedup();
        assert_eq!(all.len(), element_ids(&inserted.html).len());
    }

    #[test]
    fn duplicate_ids_are_reissued() {
        let n = norm(r#"<p data-cv-id="x">a</p><p data-cv-id="x">b</p>"#);
        let ids = element_ids(&n);
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn idgen_never_repeats() {
        let seed: Vec<String> = (1..50).map(|i| format!("e_{}", base36(i))).collect();
        let mut g = IdGen::new(&seed);
        let mut seen: HashSet<String> = seed.iter().cloned().collect();
        for _ in 0..500 {
            assert!(seen.insert(g.next()));
        }
    }

    const SAMPLE: &str = r##"<div class="root" style="color:red;margin: 0"><header><h1>Title</h1><nav><a href="#a">A</a><a href="#b">B</a></nav></header><main><section><p>one</p><p>two &amp; <b>x</b> tail</p></section><section><ul><li>1</li><li>2</li><li>3</li></ul></section></main><footer>f</footer></div>"##;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 8) as usize
        }
    }

    #[test]
    fn inverses_restore_exact_html_over_generated_ops() {
        let mut g = IdGen::new(&[]);
        let mut doc = normalize(SAMPLE, None, &mut g).unwrap().html;
        let mut rng = Rng(0x9e3779b97f4a7c15);
        let mut applied = 0;
        for _ in 0..200 {
            if applied >= 50 {
                break;
            }
            let ids = element_ids(&doc);
            if ids.is_empty() {
                doc = normalize(SAMPLE, None, &mut g).unwrap().html;
                continue;
            }
            let pick = |rng: &mut Rng| ids[rng.next() % ids.len()].clone();
            let op = match rng.next() % 6 {
                0 => DomOp::Patch {
                    el_id: pick(&mut rng),
                    attrs: vec![
                        ("class".into(), Some(format!("c{}", rng.next() % 5))),
                        (
                            "title".into(),
                            if rng.next().is_multiple_of(2) {
                                None
                            } else {
                                Some("t<\"&".into())
                            },
                        ),
                        ("onclick".into(), Some("x()".into())),
                    ],
                    style: vec![
                        ("color".into(), Some("blue".into())),
                        (
                            "margin".into(),
                            if rng.next().is_multiple_of(2) {
                                None
                            } else {
                                Some("1px".into())
                            },
                        ),
                    ],
                    text: if rng.next().is_multiple_of(2) {
                        Some("new <text> & more".into())
                    } else {
                        None
                    },
                },
                1 => DomOp::Insert {
                    parent_id: if rng.next().is_multiple_of(4) {
                        "body".into()
                    } else {
                        pick(&mut rng)
                    },
                    index: rng.next() % 4,
                    html: "<div><span>ins</span>tail</div>".into(),
                },
                2 | 3 => DomOp::Remove {
                    el_id: pick(&mut rng),
                },
                _ => DomOp::Move {
                    el_id: pick(&mut rng),
                    parent_id: pick(&mut rng),
                    index: rng.next() % 4,
                },
            };
            let Ok((after, inv)) = apply_dom_op(&doc, &op, &mut g) else {
                continue;
            };
            if let DomOp::Patch { .. } = op {
                assert!(!after.contains("onclick"));
            }
            let (restored, _) = apply_dom_op(&after, &inv, &mut g).unwrap();
            assert_eq!(restored, doc, "op {op:?} inverse {inv:?}");
            doc = after;
            applied += 1;
        }
        assert!(applied >= 50, "only {applied} ops applied");
        assert_eq!(norm(&doc), doc);
    }

    #[test]
    fn op_errors() {
        let mut g = IdGen::new(&[]);
        let doc = normalize("<div><p>a</p><br></div>", None, &mut g)
            .unwrap()
            .html;
        let ids = element_ids(&doc);
        assert!(matches!(
            apply_dom_op(
                &doc,
                &DomOp::Remove {
                    el_id: "zzz".into()
                },
                &mut g
            ),
            Err(HtmlError::NotFound(_))
        ));
        assert!(matches!(
            apply_dom_op(
                &doc,
                &DomOp::Move {
                    el_id: ids[0].clone(),
                    parent_id: ids[1].clone(),
                    index: 0
                },
                &mut g
            ),
            Err(HtmlError::BadParent(_))
        ));
        assert!(matches!(
            apply_dom_op(
                &doc,
                &DomOp::Insert {
                    parent_id: ids[2].clone(),
                    index: 0,
                    html: "<i>x</i>".into()
                },
                &mut g
            ),
            Err(HtmlError::BadParent(_))
        ));
    }
}
