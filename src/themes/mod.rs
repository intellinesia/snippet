//! Theme documents (schema 1): storage, deep base resolution, and the
//! validator shared by the HTTP routes and the agent tools.
//!
//! A theme is a JSON document: top-level metadata plus the token groups in
//! `tokens.md`. Every token is optional; an omitted token inherits from `base`,
//! deeply, down to the builtin `black`. Builtins ship in `builtin.json` (the
//! client export script regenerates it) and are seeded into the store on
//! daemon start.

use std::sync::OnceLock;

use serde_json::{Map, Value, json};

use crate::app_store::ThemeRow;
use crate::store::{Store, StoreError};

/// Groups the client honours at runtime; updated per client phase. Returned
/// with `GET /themes` so agents know what will actually show.
pub const LIVE_GROUPS: &[&str] = &[
    "color",
    "accent",
    "type.uiFont",
    "type.codeFont",
    "translucency",
    "border",
    "chat",
];

/// Accent preset names (`allAccents` in the client's `lib/theme.dart`). A
/// top-level `accent` naming one overrides the accent family at resolution.
pub const ACCENT_NAMES: &[&str] = &[
    "white",
    "violet",
    "periwinkle",
    "sky",
    "cyan",
    "emerald",
    "lime",
    "amber",
    "orange",
    "rose",
    "lavender",
];

/// UI font families (`allFonts`) and monospace families (`allMonoFonts`).
pub const UI_FONTS: &[&str] = &[
    "Cove",
    "Geist",
    "Inter",
    "PlusJakartaSans",
    "DMSans",
    "SpaceGrotesk",
    "IBMPlexSans",
    "Manrope",
    "Outfit",
    "Halden",
    "Tessera",
    "Beacon",
    "Pebble",
    "Mallow",
    "Rondel",
    "Alder",
];

pub const MONO_FONTS: &[&str] = &[
    "JetBrainsMono",
    "GeistMono",
    "FiraCode",
    "IBMPlexMono",
    "SourceCodePro",
    "Inconsolata",
    "Signal",
    "Fathom",
    "Tally",
];

/// Backdrop scene names the client knows (`ChatScene.presets` plus `none`).
pub const BACKDROP_SCENES: &[&str] = &["none", "aurora", "stars"];

const BUILTIN_JSON: &str = include_str!("builtin.json");
const DEFAULT_THEME: &str = "black";

/// The fallback theme id, public for routes that reset to it.
pub const DEFAULT_THEME_ID: &str = DEFAULT_THEME;

const TOP_KEYS: &[&str] = &[
    "schema",
    "id",
    "name",
    "base",
    "description",
    "author",
    "accent",
    "color",
    "chat",
    "type",
    "shape",
    "border",
    "translucency",
    "space",
    "motion",
    "backdrop",
    "terminal",
    "syntax",
];

// ---------------------------------------------------------------------------
// Builtins
// ---------------------------------------------------------------------------

/// Builtin theme documents from `builtin.json`. The loader accepts a top-level
/// `{"themes": [...]}` array, a bare array, or a `{id: doc}` map so the export
/// script can pick whichever shape it likes.
pub fn builtin_docs() -> &'static [Value] {
    static DOCS: OnceLock<Vec<Value>> = OnceLock::new();
    DOCS.get_or_init(|| {
        let parsed: Value = match serde_json::from_str(BUILTIN_JSON) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[themes] builtin.json does not parse: {e}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        let collect = |v: &Value, out: &mut Vec<Value>| {
            if let Some(id) = v.get("id").and_then(Value::as_str)
                && !id.is_empty()
                && v.is_object()
            {
                out.push(v.clone());
            }
        };
        match &parsed {
            Value::Array(items) => items.iter().for_each(|v| collect(v, &mut out)),
            Value::Object(map) => {
                if let Some(themes) = map.get("themes") {
                    match themes {
                        Value::Array(items) => items.iter().for_each(|v| collect(v, &mut out)),
                        Value::Object(by_id) => by_id.values().for_each(|v| collect(v, &mut out)),
                        _ => {}
                    }
                } else {
                    // A bare {id: doc} map.
                    map.values().for_each(|v| collect(v, &mut out));
                }
            }
            _ => {}
        }
        out
    })
}

fn builtin_doc(id: &str) -> Option<&'static Value> {
    builtin_docs()
        .iter()
        .find(|d| d.get("id").and_then(Value::as_str) == Some(id))
}

pub fn is_builtin_id(id: &str) -> bool {
    builtin_doc(id).is_some()
}

/// The colour tokens of the builtin `black`: the key list every `color` group
/// is validated against, so contract and builtin can't drift.
fn color_keys() -> &'static [String] {
    static KEYS: OnceLock<Vec<String>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut keys: Vec<String> = builtin_doc(DEFAULT_THEME)
            .and_then(|d| d.get("color"))
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    })
}

/// Upsert every builtin into the store. Idempotent: running it again after the
/// export script regenerates `builtin.json` refreshes the rows.
pub fn seed_builtins(store: &Store) -> Result<usize, StoreError> {
    let now = now();
    let mut seeded = 0;
    for doc in builtin_docs() {
        let Some(id) = doc.get("id").and_then(Value::as_str) else {
            continue;
        };
        let mut stored = doc.clone();
        if let Some(obj) = stored.as_object_mut() {
            obj.insert("author".into(), json!("builtin"));
        }
        let name = doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string();
        store.upsert_theme(id, &name, "builtin", true, &stored, now)?;
        seeded += 1;
    }
    Ok(seeded)
}

// ---------------------------------------------------------------------------
// Store-facing reads
// ---------------------------------------------------------------------------

/// A theme row, or a synthesized one for a builtin not yet seeded (tests and
/// tools that open the store without the daemon's start path).
pub fn get_theme(store: &Store, id: &str) -> Result<Option<ThemeRow>, StoreError> {
    if let Some(row) = store.get_theme(id)? {
        return Ok(Some(row));
    }
    Ok(builtin_doc(id).map(|doc| ThemeRow {
        id: id.to_string(),
        name: doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string(),
        author: "builtin".to_string(),
        builtin: true,
        doc: doc.clone(),
        created_at: 0,
        updated_at: 0,
    }))
}

/// Every known theme id: builtins plus store rows. Used for `base` checks.
pub fn known_theme_ids(store: &Store) -> Result<Vec<String>, StoreError> {
    let mut ids: Vec<String> = builtin_docs()
        .iter()
        .filter_map(|d| d.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    for row in store.list_themes()? {
        if !ids.contains(&row.id) {
            ids.push(row.id);
        }
    }
    Ok(ids)
}

/// The document for `id`, store first then builtins.
fn doc_for(store: &Store, id: &str) -> Result<Option<Value>, StoreError> {
    if let Some(row) = store.get_theme(id)? {
        return Ok(Some(row.doc));
    }
    Ok(builtin_doc(id).cloned())
}

/// The active theme id: the `theme_settings` row, falling back to `black` when
/// the row or the id is missing.
pub fn active_theme_id(store: &Store) -> Result<String, StoreError> {
    let Some(stored) = store.active_theme_id()? else {
        return Ok(DEFAULT_THEME.to_string());
    };
    if stored == DEFAULT_THEME {
        return Ok(DEFAULT_THEME.to_string());
    }
    if get_theme(store, &stored)?.is_some() {
        Ok(stored)
    } else {
        Ok(DEFAULT_THEME.to_string())
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Deep-merge `over` onto `base`: objects merge key by key, everything else
/// (including explicit nulls and arrays) replaces.
fn deep_merge(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out = b.clone();
            for (k, v) in o {
                out.insert(
                    k.clone(),
                    match out.get(k) {
                        Some(existing) => deep_merge(existing, v),
                        None => v.clone(),
                    },
                );
            }
            Value::Object(out)
        }
        (_, over) => over.clone(),
    }
}

fn accent_preset(name: &str) -> Option<(&'static str, &'static str)> {
    // (accent, accentFg) from `allAccents` in lib/theme.dart.
    Some(match name {
        "white" => ("#FFFFFFFF", "#FF0A0A0A"),
        "violet" => ("#FFA78BFA", "#FF140E2E"),
        "periwinkle" => ("#FF9DB0FF", "#FF10142A"),
        "sky" => ("#FF38BDF8", "#FF041A26"),
        "cyan" => ("#FF22D3EE", "#FF041E24"),
        "emerald" => ("#FF34D399", "#FF032015"),
        "lime" => ("#FFA3E635", "#FF142103"),
        "amber" => ("#FFF59E0B", "#FF1F1200"),
        "orange" => ("#FFFB923C", "#FF241002"),
        "rose" => ("#FFFB7185", "#FF28060C"),
        "lavender" => ("#FFCBA6F7", "#FF1E1030"),
        _ => return None,
    })
}

/// The eight accent-family colour fields a named preset overrides, computed
/// the way `_withAccentOverride`/`_palette` derive them in theme.dart.
fn accent_family(name: &str) -> Option<Map<String, Value>> {
    let (accent, accent_fg) = accent_preset(name)?;
    let rgb = parse_colour(accent)?;
    let hover = lighten(rgb, 0.06);
    let hover_hex = format!("{:02X}{:02X}{:02X}", hover.0, hover.1, hover.2);
    let mut out = Map::new();
    out.insert("accent".into(), json!(accent));
    out.insert("accentHover".into(), json!(format!("#FF{hover_hex}")));
    out.insert("accentFg".into(), json!(accent_fg));
    out.insert("accentBg".into(), json!(with_alpha(accent, 0.14)));
    out.insert("accentLine".into(), json!(with_alpha(accent, 0.38)));
    out.insert("accentRing".into(), json!(with_alpha(accent, 0.45)));
    out.insert("accentFill".into(), json!(accent));
    out.insert("accentFillHover".into(), json!(format!("#FF{hover_hex}")));
    Some(out)
}

/// Resolve `doc` against its base chain: every omitted token is filled from
/// the base, deeply. A top-level `accent` naming a preset then overrides the
/// accent family. Errors on a missing base or a base cycle.
pub fn resolve(store: &Store, doc: &Value) -> Result<Value, String> {
    let mut visiting = Vec::new();
    resolve_inner(store, doc, &mut visiting)
}

fn resolve_inner(store: &Store, doc: &Value, visiting: &mut Vec<String>) -> Result<Value, String> {
    let id = doc
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if visiting.iter().any(|v| v == &id) {
        return Err(format!("base cycle through `{id}`"));
    }
    visiting.push(id.clone());
    let base_doc = match doc.get("base").and_then(Value::as_str) {
        Some(base) if !base.is_empty() => {
            let found = doc_for(store, base)
                .map_err(|e| format!("store: {e}"))?
                .ok_or_else(|| format!("base `{base}` does not exist"))?;
            Some(resolve_inner(store, &found, visiting)?)
        }
        _ => None,
    };
    visiting.pop();

    let mut merged = match base_doc {
        Some(base) => deep_merge(&base, doc),
        None => doc.clone(),
    };
    if let Some(name) = merged.get("accent").and_then(Value::as_str)
        && !name.is_empty()
        && let Some(family) = accent_family(name)
        && let Some(color) = merged.get_mut("color").and_then(Value::as_object_mut)
    {
        for (k, v) in family {
            color.insert(k, v);
        }
    }
    Ok(merged)
}

// ---------------------------------------------------------------------------
// Colour math
// ---------------------------------------------------------------------------

/// `#RRGGBB` or `#AARRGGBB` → (r, g, b), ignoring alpha.
fn parse_colour(hex: &str) -> Option<(u8, u8, u8)> {
    let h = hex.strip_prefix('#')?;
    let rgb = match h.len() {
        6 => h,
        8 => &h[2..],
        _ => return None,
    };
    if !rgb.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(rgb, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn is_colour(v: &Value) -> bool {
    v.as_str().is_some_and(|s| parse_colour(s).is_some())
}

/// WCAG relative luminance of an opaque colour.
fn luminance(rgb: (u8, u8, u8)) -> f64 {
    fn channel(c: u8) -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(rgb.0) + 0.7152 * channel(rgb.1) + 0.0722 * channel(rgb.2)
}

fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// HSL lightness + `amount`, clamped — Flutter's `HSLColor.withLightness`.
fn lighten(rgb: (u8, u8, u8), amount: f64) -> (u8, u8, u8) {
    let (r, g, b) = (
        rgb.0 as f64 / 255.0,
        rgb.1 as f64 / 255.0,
        rgb.2 as f64 / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let l = (l + amount).clamp(0.0, 1.0);
    if (max - min).abs() < f64::EPSILON {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let hue = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    let hue_to_rgb = |p: f64, q: f64, mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    (
        (hue_to_rgb(p, q, hue + 1.0 / 3.0) * 255.0).round() as u8,
        (hue_to_rgb(p, q, hue) * 255.0).round() as u8,
        (hue_to_rgb(p, q, hue - 1.0 / 3.0) * 255.0).round() as u8,
    )
}

/// `#RRGGBB` recoloured to `#AARRGGBB` at `alpha` — `withValues(alpha:)`.
fn with_alpha(hex: &str, alpha: f64) -> String {
    let a = (alpha * 255.0).round() as u8;
    let rgb = hex.strip_prefix('#').unwrap_or(hex);
    let rgb = if rgb.len() == 8 { &rgb[2..] } else { rgb };
    format!("#{a:02X}{rgb}")
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn in_range(v: &Value, min: f64, max: f64) -> bool {
    v.as_f64().is_some_and(|n| n >= min && n <= max)
}

/// A number in `[min, max]`, or `{"phone": n, "desktop": n}` with both in range.
fn check_num_or_pair(v: &Value, path: &str, min: f64, max: f64, problems: &mut Vec<String>) {
    match v {
        Value::Number(_) => {
            if !in_range(v, min, max) {
                problems.push(format!("{path}: {} is outside {min} to {max}", v));
            }
        }
        Value::Object(map) => {
            let mut keys = map.keys();
            let ok_keys = map.len() == 2
                && keys.all(|k| k == "phone" || k == "desktop")
                && map.contains_key("phone")
                && map.contains_key("desktop");
            if !ok_keys {
                problems.push(format!(
                    "{path}: expected a number or {{\"phone\": n, \"desktop\": n}}"
                ));
                return;
            }
            for side in ["phone", "desktop"] {
                let Some(n) = map.get(side) else { continue };
                if !in_range(n, min, max) {
                    problems.push(format!("{path}.{side}: {} is outside {min} to {max}", n));
                }
            }
        }
        _ => problems.push(format!("{path}: expected a number or phone/desktop pair")),
    }
}

fn check_colour(v: &Value, path: &str, problems: &mut Vec<String>) {
    if !is_colour(v) {
        problems.push(format!("{path}: expected #RRGGBB or #AARRGGBB"));
    }
}

fn check_bool(v: &Value, path: &str, problems: &mut Vec<String>) {
    if !v.is_boolean() {
        problems.push(format!("{path}: expected true or false"));
    }
}

fn check_str_enum(v: &Value, path: &str, allowed: &[&str], problems: &mut Vec<String>) {
    match v.as_str() {
        Some(s) if allowed.contains(&s) => {}
        _ => problems.push(format!("{path}: expected one of {}", allowed.join(", "))),
    }
}

/// Validate the keys of `map` against `allowed`; each present key's value goes
/// through `check`. Returns nothing; problems accumulate.
fn check_group(
    map: &Map<String, Value>,
    allowed: &[&str],
    group: &str,
    problems: &mut Vec<String>,
) {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            problems.push(format!("{group}.{key}: unknown key"));
        }
    }
}

fn group_obj<'a>(
    doc: &'a Value,
    group: &str,
    problems: &mut Vec<String>,
) -> Option<&'a Map<String, Value>> {
    match doc.get(group) {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m),
        Some(_) => {
            problems.push(format!("{group}: expected an object"));
            None
        }
    }
}

fn validate_color(map: &Map<String, Value>, problems: &mut Vec<String>) {
    let keys = color_keys();
    for (k, v) in map {
        if !keys.iter().any(|c| c == k) {
            problems.push(format!("color.{k}: unknown colour token"));
            continue;
        }
        check_colour(v, &format!("color.{k}"), problems);
    }
}

fn validate_chat(map: &Map<String, Value>, problems: &mut Vec<String>) {
    const PAIRS: &[(&str, f64, f64)] = &[
        ("fontSize", 11.0, 22.0),
        ("bubbleRadius", 0.0, 32.0),
        ("maxShare", 0.5, 1.0),
        ("avatarSize", 16.0, 48.0),
    ];
    for (k, v) in map {
        let path = format!("chat.{k}");
        match k.as_str() {
            "sent" => match v.as_str() {
                Some("agent" | "accent" | "grey") => {}
                _ if is_colour(v) => {}
                _ => problems.push(format!(
                    "{path}: expected \"agent\", \"accent\", \"grey\" or a colour"
                )),
            },
            "sentText" => match v.as_str() {
                Some("auto") => {}
                _ if is_colour(v) => {}
                _ => problems.push(format!("{path}: expected \"auto\" or a colour")),
            },
            "received" | "receivedText" | "card" | "cardButton" | "meta" | "glass" => {
                check_colour(v, &path, problems)
            }
            "fontSize" | "bubbleRadius" | "maxShare" | "avatarSize" => {
                let (_, min, max) = PAIRS.iter().find(|(n, _, _)| *n == k.as_str()).unwrap();
                check_num_or_pair(v, &path, *min, *max, problems);
            }
            "runGap" | "groupGap" => {
                if !in_range(v, 0.0, 32.0) {
                    problems.push(format!("{path}: expected a number from 0 to 32"));
                }
            }
            "agentReplies" => check_str_enum(v, &path, &["bubble", "flush"], problems),
            "longReplies" => check_str_enum(v, &path, &["note", "bubble"], problems),
            _ => {}
        }
    }
}

const STYLE_NAMES: &[&str] = &[
    "pageTitle",
    "sectionTitle",
    "rowTitle",
    "body",
    "ui",
    "label",
    "meta",
    "caption",
    "overline",
    "reading",
    "code",
    "codeSmall",
];
const WEIGHT_KEYS: &[&str] = &["body", "label", "title", "strong"];
const STYLE_KEYS: &[&str] = &["size", "weight", "height", "tracking"];

fn validate_type(map: &Map<String, Value>, problems: &mut Vec<String>) {
    for (k, v) in map {
        let path = format!("type.{k}");
        match k.as_str() {
            "uiFont" => match v.as_str() {
                Some(f) if UI_FONTS.contains(&f) => {}
                _ => problems.push(format!("{path}: unknown font family")),
            },
            "codeFont" => match v.as_str() {
                Some(f) if MONO_FONTS.contains(&f) => {}
                _ => problems.push(format!("{path}: unknown monospace family")),
            },
            "scale" => {
                if !in_range(v, 0.8, 1.4) {
                    problems.push(format!("{path}: expected a number from 0.8 to 1.4"));
                }
            }
            "weights" => {
                let Some(w) = v.as_object() else {
                    problems.push(format!("{path}: expected an object"));
                    continue;
                };
                for (wk, wv) in w {
                    if !WEIGHT_KEYS.contains(&wk.as_str()) {
                        problems.push(format!("{path}.{wk}: unknown weight role"));
                        continue;
                    }
                    let ok = wv.as_f64().is_some_and(|n| {
                        n >= 100.0 && n <= 900.0 && n.fract() == 0.0 && (n as i64) % 100 == 0
                    });
                    if !ok {
                        problems.push(format!(
                            "{path}.{wk}: expected a weight from 100 to 900 in steps of 100"
                        ));
                    }
                }
            }
            "styles" => {
                let Some(styles) = v.as_object() else {
                    problems.push(format!("{path}: expected an object"));
                    continue;
                };
                for (name, style) in styles {
                    if !STYLE_NAMES.contains(&name.as_str()) {
                        problems.push(format!("{path}.{name}: unknown style name"));
                        continue;
                    }
                    let Some(sm) = style.as_object() else {
                        problems.push(format!("{path}.{name}: expected an object"));
                        continue;
                    };
                    for sk in sm.keys() {
                        if !STYLE_KEYS.contains(&sk.as_str()) {
                            problems.push(format!("{path}.{name}.{sk}: unknown key"));
                        }
                    }
                    let sp = format!("{path}.{name}");
                    if let Some(size) = sm.get("size") {
                        check_num_or_pair(size, &format!("{sp}.size"), 8.0, 48.0, problems);
                    }
                    if let Some(w) = sm.get("weight")
                        && !in_range(w, 100.0, 900.0)
                    {
                        problems.push(format!("{sp}.weight: expected a number from 100 to 900"));
                    }
                    if let Some(h) = sm.get("height")
                        && !in_range(h, 1.0, 2.0)
                    {
                        problems.push(format!("{sp}.height: expected a number from 1.0 to 2.0"));
                    }
                    if let Some(t) = sm.get("tracking")
                        && !in_range(t, -2.0, 4.0)
                    {
                        problems.push(format!("{sp}.tracking: expected a number from -2 to 4"));
                    }
                }
            }
            _ => {}
        }
    }
}

fn validate_shape(map: &Map<String, Value>, problems: &mut Vec<String>) {
    const RADII: &[(&str, f64, f64)] = &[
        ("xs", 0.0, 48.0),
        ("sm", 0.0, 48.0),
        ("chip", 0.0, 48.0),
        ("md", 0.0, 48.0),
        ("card", 0.0, 48.0),
        ("lg", 0.0, 48.0),
        ("sheetTop", 0.0, 48.0),
        ("pill", 0.0, 999.0),
    ];
    for (k, v) in map {
        let path = format!("shape.{k}");
        match k.as_str() {
            "radiusScale" => {
                if !in_range(v, 0.0, 2.0) {
                    problems.push(format!("{path}: expected a number from 0 to 2"));
                }
            }
            "radii" => {
                let Some(r) = v.as_object() else {
                    problems.push(format!("{path}: expected an object"));
                    continue;
                };
                for (rk, rv) in r {
                    let Some((_, min, max)) = RADII.iter().find(|(n, _, _)| *n == rk.as_str())
                    else {
                        problems.push(format!("{path}.{rk}: unknown radius"));
                        continue;
                    };
                    if !in_range(rv, *min, *max) {
                        problems.push(format!("{path}.{rk}: {} is outside {min} to {max}", rv));
                    }
                }
            }
            "avatar" => check_str_enum(v, &path, &["squircle", "circle"], problems),
            _ => {}
        }
    }
}

fn validate_border(map: &Map<String, Value>, problems: &mut Vec<String>) {
    for (k, v) in map {
        let path = format!("border.{k}");
        match k.as_str() {
            "width" => {
                if !in_range(v, 0.0, 3.0) {
                    problems.push(format!("{path}: expected a number from 0 to 3"));
                }
            }
            "hairline" => {
                if !in_range(v, 0.0, 2.0) {
                    problems.push(format!("{path}: expected a number from 0 to 2"));
                }
            }
            "cards" => check_bool(v, &path, problems),
            "glassEdgeAlpha" => {
                if !in_range(v, 0.0, 0.5) {
                    problems.push(format!("{path}: expected a number from 0 to 0.5"));
                }
            }
            _ => {}
        }
    }
}

fn validate_translucency(map: &Map<String, Value>, problems: &mut Vec<String>) {
    for (k, v) in map {
        let path = format!("translucency.{k}");
        match k.as_str() {
            "canvasAlpha" | "glassFill" => check_num_or_pair(v, &path, 0.3, 1.0, problems),
            "windowTint" => {
                if !in_range(v, 0.0, 1.0) {
                    problems.push(format!("{path}: expected a number from 0 to 1"));
                }
            }
            "glassBlur" => {
                if !in_range(v, 0.0, 60.0) {
                    problems.push(format!("{path}: expected a number from 0 to 60"));
                }
            }
            "edgeBlur" | "enabled" => check_bool(v, &path, problems),
            "scrim" | "glassScrim" | "modalScrim" => {
                if !in_range(v, 0.0, 0.9) {
                    problems.push(format!("{path}: expected a number from 0 to 0.9"));
                }
            }
            _ => {}
        }
    }
}

const SCALE_KEYS: &[&str] = &[
    "s2", "s4", "s6", "s8", "s12", "s16", "s20", "s24", "s32", "s40",
];
const METRIC_KEYS: &[&str] = &[
    "gutter",
    "navRow",
    "minTarget",
    "appBarHeight",
    "rowHeight",
    "rowPadH",
    "tabStripHeight",
    "tabIconSize",
    "tabActionSize",
];

fn validate_space(map: &Map<String, Value>, problems: &mut Vec<String>) {
    for (k, v) in map {
        let path = format!("space.{k}");
        match k.as_str() {
            "density" => {
                if !in_range(v, 0.75, 1.4) {
                    problems.push(format!("{path}: expected a number from 0.75 to 1.4"));
                }
            }
            "scale" => {
                let Some(s) = v.as_object() else {
                    problems.push(format!("{path}: expected an object"));
                    continue;
                };
                for (sk, sv) in s {
                    if !SCALE_KEYS.contains(&sk.as_str()) {
                        problems.push(format!("{path}.{sk}: unknown spacing step"));
                        continue;
                    }
                    if !in_range(sv, 0.0, 64.0) {
                        problems.push(format!("{path}.{sk}: expected a number from 0 to 64"));
                    }
                }
            }
            "metrics" => {
                let Some(m) = v.as_object() else {
                    problems.push(format!("{path}: expected an object"));
                    continue;
                };
                for (mk, mv) in m {
                    if !METRIC_KEYS.contains(&mk.as_str()) {
                        problems.push(format!("{path}.{mk}: unknown metric"));
                        continue;
                    }
                    check_num_or_pair(mv, &format!("{path}.{mk}"), 0.0, 120.0, problems);
                }
            }
            _ => {}
        }
    }
}

const DURATION_KEYS: &[&str] = &[
    "press", "quick", "fast", "base", "open", "close", "page", "slow", "pulse",
];

fn validate_motion(map: &Map<String, Value>, problems: &mut Vec<String>) {
    for (k, v) in map {
        let path = format!("motion.{k}");
        match k.as_str() {
            "speed" => {
                if !in_range(v, 0.0, 3.0) {
                    problems.push(format!("{path}: expected a number from 0 to 3"));
                }
            }
            "durations" => {
                let Some(d) = v.as_object() else {
                    problems.push(format!("{path}: expected an object"));
                    continue;
                };
                for (dk, dv) in d {
                    if !DURATION_KEYS.contains(&dk.as_str()) {
                        problems.push(format!("{path}.{dk}: unknown duration"));
                        continue;
                    }
                    if !in_range(dv, 0.0, 2000.0) {
                        problems.push(format!("{path}.{dk}: expected milliseconds from 0 to 2000"));
                    }
                }
            }
            "curves" => {
                let Some(c) = v.as_object() else {
                    problems.push(format!("{path}: expected an object"));
                    continue;
                };
                for (ck, cv) in c {
                    if ck != "enter" && ck != "exit" {
                        problems.push(format!("{path}.{ck}: unknown curve"));
                        continue;
                    }
                    let ok = cv
                        .as_array()
                        .is_some_and(|a| a.len() == 4 && a.iter().all(|n| in_range(n, -1.0, 2.0)));
                    if !ok {
                        problems.push(format!(
                            "{path}.{ck}: expected [x1, y1, x2, y2] with each value -1 to 2"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

fn valid_key_name(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 64
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn validate_backdrop(map: &Map<String, Value>, problems: &mut Vec<String>) {
    for (k, v) in map {
        let path = format!("backdrop.{k}");
        match k.as_str() {
            "scene" => match v.as_str() {
                Some(s) if BACKDROP_SCENES.contains(&s) => {}
                _ => problems.push(format!("{path}: unknown scene")),
            },
            "params" => {
                let Some(p) = v.as_object() else {
                    problems.push(format!("{path}: expected an object"));
                    continue;
                };
                if p.len() > 16 {
                    problems.push(format!("{path}: at most 16 params"));
                }
                for (pk, pv) in p {
                    if !valid_key_name(pk) {
                        problems.push(format!("{path}.{pk}: invalid param name"));
                        continue;
                    }
                    let ok = match pv {
                        Value::Number(_) => in_range(pv, -1000.0, 1000.0),
                        _ => is_colour(pv),
                    };
                    if !ok {
                        problems.push(format!(
                            "{path}.{pk}: expected a number -1000 to 1000 or a colour"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

fn validate_terminal(map: &Map<String, Value>, problems: &mut Vec<String>) {
    for (k, v) in map {
        let path = format!("terminal.{k}");
        match k.as_str() {
            "ansi" => {
                let Some(a) = v.as_array() else {
                    problems.push(format!("{path}: expected an array of 16 colours"));
                    continue;
                };
                if a.len() != 16 {
                    problems.push(format!("{path}: expected 16 colours, got {}", a.len()));
                }
                for (i, c) in a.iter().enumerate() {
                    if !is_colour(c) {
                        problems.push(format!("{path}[{i}]: expected #RRGGBB or #AARRGGBB"));
                    }
                }
            }
            _ => {}
        }
    }
}

fn validate_syntax(map: &Map<String, Value>, problems: &mut Vec<String>) {
    for (scope, colour) in map {
        if !valid_key_name(scope) {
            problems.push(format!("syntax.{scope}: invalid scope name"));
            continue;
        }
        check_colour(colour, &format!("syntax.{scope}"), problems);
    }
}

// ---------------------------------------------------------------------------
// Resolved-theme checks
// ---------------------------------------------------------------------------

/// The smaller of a number or a phone/desktop pair — the conservative read for
/// minimum-size checks.
fn pair_min(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::Object(m) => ["phone", "desktop"]
            .iter()
            .filter_map(|k| m.get(*k).and_then(Value::as_f64))
            .reduce(f64::min),
        _ => None,
    }
}

fn colour_at<'a>(doc: &'a Value, path: &[&str]) -> Option<(u8, u8, u8)> {
    let mut cur = doc;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_str().and_then(parse_colour)
}

fn contrast_problem(name: &str, bg: &str, ratio: f64, need: f64) -> String {
    format!("{name} on {bg} is {ratio:.1}:1, needs {need}:1")
}

/// Dark-only, contrast and min-target checks on the resolved theme.
fn resolved_problems(resolved: &Value, problems: &mut Vec<String>) {
    if let Some(bg) = colour_at(resolved, &["color", "bg"]) {
        let lum = luminance(bg);
        if lum > 0.08 {
            problems.push(format!(
                "color.bg: relative luminance {lum:.3} is above 0.08 (dark themes only)"
            ));
        }
    }
    let pairs: &[(&[&str], &[&str], &str, &str, f64)] = &[
        (&["color", "fg1"], &["color", "bg"], "fg1", "bg", 7.0),
        (
            &["color", "fg2"],
            &["color", "surface1"],
            "fg2",
            "surface1",
            4.5,
        ),
        (
            &["color", "fg3"],
            &["color", "surface1"],
            "fg3",
            "surface1",
            3.0,
        ),
        (
            &["color", "accentFg"],
            &["color", "accent"],
            "accentFg",
            "accent",
            3.0,
        ),
    ];
    for &(fg_path, bg_path, fg_name, bg_name, need) in pairs {
        let (Some(fg), Some(bg)) = (colour_at(resolved, fg_path), colour_at(resolved, bg_path))
        else {
            continue;
        };
        let ratio = contrast_ratio(fg, bg);
        if ratio + 1e-9 < need {
            problems.push(contrast_problem(fg_name, bg_name, ratio, need));
        }
    }
    // chat.receivedText on chat.received.
    if let (Some(fg), Some(bg)) = (
        colour_at(resolved, &["chat", "receivedText"]),
        colour_at(resolved, &["chat", "received"]),
    ) {
        let ratio = contrast_ratio(fg, bg);
        if ratio + 1e-9 < 4.5 {
            problems.push(contrast_problem(
                "chat.receivedText",
                "chat.received",
                ratio,
                4.5,
            ));
        }
    }
    // chat.sentText on chat.sent, only when both are literal colours.
    if let (Some(fg), Some(bg)) = (
        colour_at(resolved, &["chat", "sentText"]),
        colour_at(resolved, &["chat", "sent"]),
    ) {
        let ratio = contrast_ratio(fg, bg);
        if ratio + 1e-9 < 4.5 {
            problems.push(contrast_problem("chat.sentText", "chat.sent", ratio, 4.5));
        }
    }
    // minTarget after density.
    let density = resolved
        .get("space")
        .and_then(|s| s.get("density"))
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    if let Some(min_target) = pair_min(
        resolved
            .get("space")
            .and_then(|s| s.get("metrics"))
            .and_then(|m| m.get("minTarget")),
    ) {
        let effective = min_target * density;
        if effective + 1e-9 < 40.0 {
            problems.push(format!(
                "space.metrics.minTarget: {effective:.1} after density is below 40"
            ));
        }
    }
}

/// Validate a theme document. `is_builtin` relaxes the `base` requirement
/// (builtins set every token and have no base). Returns every problem found,
/// each `"<path>: <reason>"` or a bare contrast/dark line.
pub fn validate(store: &Store, doc: &Value, is_builtin: bool) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(obj) = doc.as_object() else {
        return vec!["<root>: expected a JSON object".to_string()];
    };

    for key in obj.keys() {
        if !TOP_KEYS.contains(&key.as_str()) {
            problems.push(format!("{key}: unknown top-level key"));
        }
    }

    match obj.get("schema") {
        Some(Value::Number(n)) if n.as_i64() == Some(1) => {}
        _ => problems.push("schema: must be 1".to_string()),
    }
    match obj.get("id").and_then(Value::as_str) {
        Some(id)
            if (2..=48).contains(&id.len())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') => {}
        _ => problems.push("id: must match [a-z0-9-]{2,48}".to_string()),
    }
    match obj.get("name").and_then(Value::as_str) {
        Some(name) if !name.trim().is_empty() && name.chars().count() <= 40 => {}
        _ => problems.push("name: must be 1 to 40 characters".to_string()),
    }
    if let Some(base) = obj.get("base") {
        match base.as_str() {
            Some(b) if !b.is_empty() => match known_theme_ids(store) {
                Ok(ids) if ids.iter().any(|i| i.as_str() == b) => {}
                Ok(_) => problems.push(format!("base: unknown theme `{b}`")),
                Err(e) => problems.push(format!("base: store error: {e}")),
            },
            _ => problems.push("base: must be a theme id".to_string()),
        }
    } else if !is_builtin {
        problems.push("base: required (builtins are the only baseless themes)".to_string());
    }
    if let Some(d) = obj.get("description")
        && (!d.is_string() || d.as_str().is_some_and(|s| s.chars().count() > 200))
    {
        problems.push("description: must be a string of at most 200 characters".to_string());
    }
    if let Some(a) = obj.get("accent")
        && !a.is_null()
        && !a.as_str().is_some_and(|s| ACCENT_NAMES.contains(&s))
    {
        problems.push("accent: must be an accent preset name or null".to_string());
    }

    if let Some(map) = group_obj(doc, "color", &mut problems) {
        let keys: Vec<&str> = color_keys().iter().map(String::as_str).collect();
        check_group(map, &keys, "color", &mut problems);
        validate_color(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "chat", &mut problems) {
        check_group(
            map,
            &[
                "sent",
                "sentText",
                "received",
                "receivedText",
                "card",
                "cardButton",
                "meta",
                "glass",
                "fontSize",
                "bubbleRadius",
                "runGap",
                "groupGap",
                "maxShare",
                "avatarSize",
                "agentReplies",
                "longReplies",
            ],
            "chat",
            &mut problems,
        );
        validate_chat(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "type", &mut problems) {
        check_group(
            map,
            &["uiFont", "codeFont", "scale", "weights", "styles"],
            "type",
            &mut problems,
        );
        validate_type(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "shape", &mut problems) {
        check_group(
            map,
            &["radiusScale", "radii", "avatar"],
            "shape",
            &mut problems,
        );
        validate_shape(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "border", &mut problems) {
        check_group(
            map,
            &["width", "hairline", "cards", "glassEdgeAlpha"],
            "border",
            &mut problems,
        );
        validate_border(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "translucency", &mut problems) {
        check_group(
            map,
            &[
                "canvasAlpha",
                "windowTint",
                "glassFill",
                "glassBlur",
                "edgeBlur",
                "scrim",
                "glassScrim",
                "modalScrim",
                "enabled",
            ],
            "translucency",
            &mut problems,
        );
        validate_translucency(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "space", &mut problems) {
        check_group(
            map,
            &["density", "scale", "metrics"],
            "space",
            &mut problems,
        );
        validate_space(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "motion", &mut problems) {
        check_group(
            map,
            &["speed", "durations", "curves"],
            "motion",
            &mut problems,
        );
        validate_motion(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "backdrop", &mut problems) {
        check_group(map, &["scene", "params"], "backdrop", &mut problems);
        validate_backdrop(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "terminal", &mut problems) {
        check_group(map, &["ansi"], "terminal", &mut problems);
        validate_terminal(map, &mut problems);
    }
    if let Some(map) = group_obj(doc, "syntax", &mut problems) {
        validate_syntax(map, &mut problems);
    }

    // Resolved checks run whenever the document can be resolved; unparseable
    // colours are skipped here because the token checks already reported them.
    let base_ok = is_builtin
        || match obj.get("base").and_then(Value::as_str) {
            Some(b) if !b.is_empty() => known_theme_ids(store)
                .map(|ids| ids.iter().any(|i| i.as_str() == b))
                .unwrap_or(false),
            _ => false,
        };
    if base_ok || is_builtin {
        match resolve(store, doc) {
            Ok(resolved) => resolved_problems(&resolved, &mut problems),
            Err(e) => problems.push(format!("base: {e}")),
        }
    }

    problems
}

// ---------------------------------------------------------------------------
// Route/tool-facing summaries
// ---------------------------------------------------------------------------

/// The token contract as JSON, for `theme_schema`: every group's keys, types
/// and ranges plus the live groups.
pub fn contract() -> Value {
    json!({
        "schema": 1,
        "top_level": {
            "schema": "must be 1",
            "id": "slug [a-z0-9-]{2,48}",
            "name": "string, 1 to 40 chars",
            "base": "theme id; required unless builtin",
            "description": "string up to 200 chars, optional",
            "author": "set by the daemon: builtin, user, or agent:<id>",
            "accent": "AccentPreset name or null; overrides the accent colours at resolution",
        },
        "groups": {
            "color": {"keys": color_keys(), "values": "#RRGGBB or #AARRGGBB"},
            "chat": {
                "sent": "\"agent\" | \"accent\" | \"grey\" | colour",
                "sentText": "\"auto\" | colour",
                "received|receivedText|card|cardButton|meta|glass": "colour",
                "fontSize": "number|{phone,desktop} 11-22",
                "bubbleRadius": "number|{phone,desktop} 0-32",
                "runGap|groupGap": "0-32",
                "maxShare": "number|{phone,desktop} 0.5-1.0",
                "avatarSize": "number|{phone,desktop} 16-48",
                "agentReplies": "\"bubble\" | \"flush\"",
                "longReplies": "\"note\" | \"bubble\"",
            },
            "type": {
                "uiFont": UI_FONTS,
                "codeFont": MONO_FONTS,
                "scale": "0.8-1.4",
                "weights": "{body,label,title,strong}: 100-900 step 100",
                "styles": format!("{}: {{size: number|pair 8-48, weight: 100-900, height: 1.0-2.0, tracking: -2-4}}", STYLE_NAMES.join("|")),
            },
            "shape": {
                "radiusScale": "0-2",
                "radii": "{xs,sm,chip,md,card,lg,sheetTop}: 0-48, pill: 0-999",
                "avatar": "\"squircle\" | \"circle\"",
            },
            "border": {"width": "0-3", "hairline": "0-2", "cards": "bool", "glassEdgeAlpha": "0-0.5"},
            "translucency": {
                "canvasAlpha": "number|pair 0.3-1",
                "windowTint": "0-1",
                "glassFill": "number|pair 0.3-1",
                "glassBlur": "0-60",
                "edgeBlur": "bool",
                "scrim|glassScrim|modalScrim": "0-0.9",
                "enabled": "bool",
            },
            "space": {
                "density": "0.75-1.4",
                "scale": "{s2,s4,s6,s8,s12,s16,s20,s24,s32,s40}: 0-64",
                "metrics": "{gutter,navRow,minTarget,appBarHeight,rowHeight,rowPadH,tabStripHeight,tabIconSize,tabActionSize}: number|pair 0-120; minTarget >= 40 after density",
            },
            "motion": {
                "speed": "0-3 (0 disables animation; larger is slower)",
                "durations": "{press,quick,fast,base,open,close,page,slow,pulse}: ms 0-2000",
                "curves": "{enter,exit}: [x1,y1,x2,y2], each -1 to 2",
            },
            "backdrop": {"scene": BACKDROP_SCENES, "params": "object of numbers (-1000..1000) and colours, at most 16 keys"},
            "terminal": {"ansi": "array of exactly 16 colours"},
            "syntax": "map of highlight scope name to colour",
        },
        "resolved_checks": [
            "color.bg relative luminance <= 0.08 (dark only)",
            "fg1 on bg >= 7:1, fg2 on surface1 >= 4.5:1, fg3 on surface1 >= 3:1, accentFg on accent >= 3:1",
            "chat.receivedText on chat.received >= 4.5:1; chat.sentText on chat.sent >= 4.5:1 when both are colours",
            "space.metrics.minTarget >= 40 after density",
        ],
        "live_groups": LIVE_GROUPS,
    })
}

/// Preview colours for `GET /themes`, from the resolved document.
pub fn preview(resolved: &Value) -> Value {
    let pick = |path: &[&str]| -> Value {
        let mut cur = resolved;
        for key in path {
            match cur.get(*key) {
                Some(v) => cur = v,
                None => return Value::Null,
            }
        }
        cur.clone()
    };
    json!({
        "bg": pick(&["color", "bg"]),
        "surface1": pick(&["color", "surface1"]),
        "fg1": pick(&["color", "fg1"]),
        "accent": pick(&["color", "accent"]),
    })
}

pub fn emit_changed(active: &str, changed: &str) {
    crate::session::emit_device_event(json!({
        "kind": "themes_changed",
        "active": active,
        "changed": changed,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn black() -> Value {
        builtin_doc("black").cloned().expect("black builtin")
    }

    /// A minimal valid non-builtin document based on `black`.
    fn child_doc(id: &str) -> Value {
        json!({
            "schema": 1,
            "id": id,
            "name": "Child",
            "base": "black",
        })
    }

    #[test]
    fn every_builtin_validates() {
        let store = store();
        for doc in builtin_docs() {
            let id = doc.get("id").and_then(Value::as_str).unwrap_or("?");
            let problems = validate(&store, doc, true);
            assert!(problems.is_empty(), "builtin `{id}`: {problems:?}");
        }
    }

    #[test]
    fn schema_must_be_1() {
        let store = store();
        let mut doc = child_doc("bad-schema");
        doc["schema"] = json!(2);
        let problems = validate(&store, &doc, false);
        assert!(
            problems.iter().any(|p| p.starts_with("schema:")),
            "{problems:?}"
        );
    }

    #[test]
    fn id_must_be_a_slug() {
        let store = store();
        for bad in ["AB", "has space", "under_score", &"a".repeat(49), ""] {
            let mut doc = child_doc("ok-id");
            doc["id"] = json!(bad);
            let problems = validate(&store, &doc, false);
            assert!(
                problems.iter().any(|p| p.starts_with("id:")),
                "id `{bad}`: {problems:?}"
            );
        }
    }

    #[test]
    fn base_is_required_and_must_exist() {
        let store = store();
        let mut doc = child_doc("no-base");
        doc.as_object_mut().unwrap().remove("base");
        let problems = validate(&store, &doc, false);
        assert!(
            problems.iter().any(|p| p.starts_with("base:")),
            "{problems:?}"
        );

        let mut doc = child_doc("ghost");
        doc["base"] = json!("no-such-theme");
        let problems = validate(&store, &doc, false);
        assert!(
            problems.iter().any(|p| p.contains("unknown theme")),
            "{problems:?}"
        );
    }

    #[test]
    fn unknown_keys_rejected_at_every_level() {
        let store = store();
        let mut doc = child_doc("keys");
        doc["bogus"] = json!(1);
        doc["color"] = json!({"notAColour": "#FFFFFF", "bg": "#000000"});
        doc["shape"] = json!({"radii": {"card": 10, "oval": 3}});
        doc["type"] = json!({"styles": {"hero": {"size": 12}}});
        doc["motion"] =
            json!({"curves": {"enter": [0.0, 0.0, 1.0, 1.0], "spin": [0.0, 0.0, 1.0, 1.0]}});
        let problems = validate(&store, &doc, false);
        for needle in [
            "bogus: unknown top-level key",
            "color.notAColour",
            "shape.radii.oval",
            "type.styles.hero",
            "motion.curves.spin",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(needle)),
                "missing `{needle}` in {problems:?}"
            );
        }
    }

    #[test]
    fn num_or_pair_accepted_and_checked() {
        let store = store();
        let mut doc = child_doc("pairs");
        doc["chat"] = json!({
            "fontSize": {"phone": 16, "desktop": 13.5},
            "bubbleRadius": 18,
            "avatarSize": {"phone": 28},
        });
        let problems = validate(&store, &doc, false);
        assert!(
            problems.iter().any(|p| p.contains("chat.avatarSize")),
            "{problems:?}"
        );
        assert!(
            !problems
                .iter()
                .any(|p| p.contains("chat.fontSize") || p.contains("chat.bubbleRadius")),
            "{problems:?}"
        );
    }

    #[test]
    fn dark_only_rejects_light_bg() {
        let store = store();
        let mut doc = black();
        doc["color"]["bg"] = json!("#FFFFFFFF");
        let problems = validate(&store, &doc, true);
        assert!(
            problems.iter().any(|p| p.starts_with("color.bg:")),
            "{problems:?}"
        );
    }

    #[test]
    fn contrast_rejects_low_fg2() {
        let store = store();
        let mut doc = black();
        // Nearly the same grey as surface1 (#0F0F0F): far below 4.5:1.
        doc["color"]["fg2"] = json!("#FF141414");
        let problems = validate(&store, &doc, true);
        assert!(
            problems.iter().any(|p| p.starts_with("fg2 on surface1")),
            "{problems:?}"
        );
    }

    #[test]
    fn accent_preset_overrides_family_on_resolve() {
        let store = store();
        let mut doc = child_doc("tinted");
        doc["accent"] = json!("violet");
        let resolved = resolve(&store, &doc).unwrap();
        assert_eq!(resolved["color"]["accent"], json!("#FFA78BFA"));
        assert_eq!(resolved["color"]["accentFg"], json!("#FF140E2E"));
        assert_eq!(resolved["color"]["accentBg"], json!("#24A78BFA"));
    }

    #[test]
    fn base_resolution_fills_and_overrides_deeply() {
        let store = store();
        let mut doc = child_doc("deep");
        doc["color"] = json!({"surface1": "#FF111111", "fg4": "#FFEEEEEE"});
        doc["shape"] = json!({"radii": {"card": 20}});
        let resolved = resolve(&store, &doc).unwrap();
        // Overrides landed…
        assert_eq!(resolved["color"]["surface1"], json!("#FF111111"));
        assert_eq!(resolved["shape"]["radii"]["card"], json!(20));
        // …and everything omitted came from black, per key within a group.
        assert_eq!(resolved["color"]["bg"], json!("#000000"));
        assert_eq!(resolved["color"]["fg1"], black()["color"]["fg1"]);
        assert_eq!(resolved["shape"]["radii"]["lg"].as_f64(), Some(12.0));
        assert!(validate(&store, &doc, false).is_empty());
    }

    #[test]
    fn min_target_checked_after_density() {
        let store = store();
        let mut doc = child_doc("tiny-target");
        doc["space"] = json!({
            "density": 0.75,
            "metrics": {"minTarget": 52},
        });
        // 52 * 0.75 = 39 < 40.
        let problems = validate(&store, &doc, false);
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("space.metrics.minTarget")),
            "{problems:?}"
        );
        doc["space"]["metrics"]["minTarget"] = json!(54);
        let problems = validate(&store, &doc, false);
        assert!(
            !problems
                .iter()
                .any(|p| p.starts_with("space.metrics.minTarget")),
            "{problems:?}"
        );
    }

    #[test]
    fn base_cycle_is_a_problem() {
        let store = store();
        // Two stored docs pointing at each other.
        let a = json!({"schema":1,"id":"cyc-a","name":"A","base":"cyc-b"});
        let b = json!({"schema":1,"id":"cyc-b","name":"B","base":"cyc-a"});
        let now = now();
        store
            .upsert_theme("cyc-a", "A", "user", false, &a, now)
            .unwrap();
        store
            .upsert_theme("cyc-b", "B", "user", false, &b, now)
            .unwrap();
        let problems = validate(&store, &a, false);
        assert!(problems.iter().any(|p| p.contains("cycle")), "{problems:?}");
    }
}
