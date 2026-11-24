use canvas_render::{ElementRect, RenderError, hit_test, render};

const FLEX: &str = include_str!("frames/flex_cards.html");
const GRID: &str = include_str!("frames/grid_dashboard.html");
const TEXT: &str = include_str!("frames/text_radii_shadow.html");

fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = dec.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

fn non_white_share(rgba: &[u8]) -> f32 {
    let px = rgba.len() / 4;
    let n = rgba
        .chunks_exact(4)
        .filter(|p| p[0] < 250 || p[1] < 250 || p[2] < 250)
        .count();
    n as f32 / px as f32
}

fn find<'a>(layout: &'a [ElementRect], id: &str) -> &'a ElementRect {
    layout
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("{id} missing from layout"))
}

fn assert_rect(r: &ElementRect, x: f32, y: f32, w: f32, h: f32) {
    let close = |a: f32, b: f32| (a - b).abs() <= 2.0;
    assert!(
        close(r.x, x) && close(r.y, y) && close(r.w, w) && close(r.h, h),
        "{} got [{}, {}, {}, {}] want ~[{x}, {y}, {w}, {h}]",
        r.id,
        r.x,
        r.y,
        r.w,
        r.h
    );
}

fn inside(child: &ElementRect, parent: &ElementRect) {
    assert!(
        child.x >= parent.x - 0.5
            && child.y >= parent.y - 0.5
            && child.x + child.w <= parent.x + parent.w + 0.5
            && child.y + child.h <= parent.y + parent.h + 0.5,
        "{} not inside {}",
        child.id,
        parent.id
    );
}

#[test]
fn flex_cards() {
    let r = render(FLEX, 800, 0, 2.0).unwrap();
    let (w, h, px) = decode(&r.png);
    assert_eq!((w, h), (r.width, r.height));
    assert_eq!(w, 1600);
    assert!(h >= 300 && h <= 400, "content height {h}");
    assert!(non_white_share(&px) > 0.3);

    let ids: Vec<&str> = r.layout.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["row", "c1", "c1-title", "c1-btn", "c2", "c2-btn", "c3", "c3-btn"]);
    let row = find(&r.layout, "row");
    assert_rect(row, 24.0, 24.0, 752.0, row.h);
    let (c1, c2, c3) = (find(&r.layout, "c1"), find(&r.layout, "c2"), find(&r.layout, "c3"));
    assert!((c1.w - c2.w).abs() < 1.0 && (c2.w - c3.w).abs() < 1.0);
    assert!(c1.x < c2.x && c2.x < c3.x && c1.y == c2.y);
    for c in [c1, c2, c3] {
        inside(c, row);
    }
    let title = find(&r.layout, "c1-title");
    assert_eq!(title.tag, "h3");
    assert_eq!(title.text.as_deref(), Some("Starter"));
    inside(find(&r.layout, "c1-btn"), c1);

    let btn = find(&r.layout, "c2-btn");
    let hit = hit_test(FLEX, 800, btn.x + btn.w / 2.0, btn.y + btn.h / 2.0);
    assert_eq!(hit.as_deref(), Some("c2-btn"));
    assert_eq!(hit_test(FLEX, 800, c3.x + 4.0, c3.y + 4.0).as_deref(), Some("c3"));
    assert_eq!(hit_test(FLEX, 800, 5.0, 5.0), None);
}

#[test]
fn grid_dashboard() {
    let r = render(GRID, 800, 600, 1.0).unwrap();
    let (w, h, px) = decode(&r.png);
    assert_eq!((w, h), (800, 600));
    assert!(non_white_share(&px) > 0.9);

    assert_rect(find(&r.layout, "side"), 0.0, 0.0, 200.0, 600.0);
    assert_rect(find(&r.layout, "main"), 200.0, 0.0, 600.0, 600.0);
    let rev = find(&r.layout, "t-revenue");
    let chart = find(&r.layout, "t-chart");
    let log = find(&r.layout, "t-log");
    assert_rect(rev, 224.0, 24.0, rev.w, 120.0);
    assert!((chart.w - (2.0 * rev.w + 16.0)).abs() < 2.0, "span 2 width {}", chart.w);
    assert!((chart.y - (rev.y + 136.0)).abs() < 1.0);
    assert_eq!(log.y, chart.y);
    assert_eq!(find(&r.layout, "t-users").text.as_deref(), Some("Users"));

    assert_eq!(hit_test(GRID, 800, chart.x + 10.0, chart.y + 80.0).as_deref(), Some("t-chart"));
    assert_eq!(hit_test(GRID, 800, 40.0, 38.0).as_deref(), Some("nav-home"));
    assert_eq!(hit_test(GRID, 800, 100.0, 400.0).as_deref(), Some("side"));
}

#[test]
fn text_radii_shadow() {
    let r = render(TEXT, 800, 0, 3.0).unwrap();
    let (w, h, px) = decode(&r.png);
    assert_eq!(w, 2400);
    assert_eq!(h, r.height);
    assert!(non_white_share(&px) > 0.3);

    let hero = find(&r.layout, "hero");
    assert_rect(hero, 40.0, 40.0, 720.0, hero.h);
    assert!(hero.h > 250.0 && hero.h < 500.0);
    for id in ["title", "body", "code", "pill", "avatar", "remote", "dot"] {
        inside(find(&r.layout, id), hero);
    }
    let code = find(&r.layout, "code");
    let body = find(&r.layout, "body");
    assert!(code.w > 50.0 && code.h > 10.0, "inline code rect {code:?}");
    assert!(code.x > body.x + 100.0);
    assert_eq!(code.text.as_deref(), Some("inline code"));
    assert_eq!(find(&r.layout, "avatar").w, 64.0);
    assert_eq!(find(&r.layout, "dot").w, 20.0);

    let dot = find(&r.layout, "dot");
    let (dx, dy) = (((dot.x + 10.0) * 3.0) as usize, ((dot.y + 10.0) * 3.0) as usize);
    let i = (dy * w as usize + dx) * 4;
    assert!(px[i + 1] > 200 && px[i] < 200, "data: image not painted: {:?}", &px[i..i + 4]);

    assert_eq!(hit_test(TEXT, 800, code.x + code.w / 2.0, code.y + code.h / 2.0).as_deref(), Some("code"));
    let pill = find(&r.layout, "pill");
    assert_eq!(hit_test(TEXT, 800, pill.x + 20.0, pill.y + 18.0).as_deref(), Some("pill"));
    assert_eq!(hit_test(TEXT, 800, hero.x + 600.0, hero.y + 300.0).as_deref(), Some("hero"));
}

#[test]
fn limits() {
    assert_eq!(render(FLEX, 4001, 100, 1.0).unwrap_err(), RenderError::TooLarge);
    assert_eq!(render(FLEX, 800, 8001, 1.0).unwrap_err(), RenderError::TooLarge);
    assert!(matches!(render(FLEX, 0, 100, 1.0), Err(RenderError::Layout(_))));
    let tall = format!("<div data-cv-id=\"t\" style=\"height:20000px;background:#000\"></div>");
    let r = render(&tall, 100, 0, 1.0).unwrap();
    assert_eq!(r.height, 8000);
}
