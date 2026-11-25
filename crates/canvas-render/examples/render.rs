use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: render in.html out.png [width] [scale] [height] [iters]");
        std::process::exit(2);
    }
    let html = std::fs::read_to_string(&args[1]).expect("read html");
    let width: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(800);
    let scale: f32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let height: u32 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);
    let iters: usize = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(1).max(1);

    let mut times = Vec::with_capacity(iters);
    let mut last = None;
    for _ in 0..iters {
        let t = Instant::now();
        let r = canvas_render::render(&html, width, height, scale);
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        last = Some(r);
    }
    let rendered = match last.unwrap() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    std::fs::write(&args[2], &rendered.png).expect("write png");

    let layout: Vec<_> = rendered
        .layout
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id, "tag": r.tag, "x": r.x, "y": r.y, "w": r.w, "h": r.h, "text": r.text,
            })
        })
        .collect();
    let out = serde_json::json!({
        "width": rendered.width,
        "height": rendered.height,
        "layout": layout,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());

    if iters > 1 {
        let mut warm = times[1..].to_vec();
        warm.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| warm[((warm.len() as f64 - 1.0) * p).round() as usize];
        eprintln!(
            "render_ms first={:.1} p50={:.1} p95={:.1} n={}",
            times[0],
            pct(0.5),
            pct(0.95),
            warm.len()
        );
    }
}
