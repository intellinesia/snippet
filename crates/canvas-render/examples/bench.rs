use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let frames: Vec<String> = if args.is_empty() {
        ["flex_cards", "grid_dashboard", "text_radii_shadow"]
            .iter()
            .map(|f| format!("{}/tests/frames/{f}.html", env!("CARGO_MANIFEST_DIR")))
            .collect()
    } else {
        args
    };
    let htmls: Vec<String> = frames.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect();
    for h in &htmls {
        canvas_render::render(h, 800, 0, 2.0).unwrap();
    }
    let mut times = Vec::new();
    for _ in 0..50 {
        for h in &htmls {
            let t = Instant::now();
            canvas_render::render(h, 800, 0, 2.0).unwrap();
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| times[((times.len() as f64 - 1.0) * p).round() as usize];
    println!("800px @2x, warm, n={}: p50={:.1} ms p95={:.1} ms", times.len(), pct(0.5), pct(0.95));
}
