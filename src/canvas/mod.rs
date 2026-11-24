//! Agent-first canvas backend: store, append-only op log, snapshots, pins, links, and renderer trait.

pub mod html;
pub mod links;
pub mod model;
pub mod render;
pub mod store;
pub mod tools;

pub use links::{
    CanvasLink, LinkError, extract_markdown_links, format_markdown_link, parse_canvas_ref,
};
pub use model::{
    Author, AuthorKind, Batch, BatchCommit, CANVAS_FORMAT_VERSION, CanvasError, CanvasSnapshot,
    DEFAULT_FRAME_H, DEFAULT_FRAME_W, Frame, Op, Pin, PinStatus, PinsFile,
};
pub use render::{BlitzRenderer, ElementRect, FrameRenderer, RenderError, Rendered, StubRenderer};
pub use store::{
    CachedLayout, CanvasService, CanvasStore, HIT_TEST_TIMEOUT, RENDER_TIMEOUT, SNAPSHOT_INTERVAL,
    THUMB_LINK_TTL_SECS, canvas_id_for_workspace, canvases_root, emit_canvas_rev_event,
    normalize_scale, signed_thumb_path, verify_signed_thumb,
};
pub use tools::{
    CANVAS_HTML_SUBSET_NOTE, CanvasApplyTool, CanvasPinsTool, CanvasReadTool, CanvasThumbnailTool,
    MAX_CANVAS_READ_BYTES, add_canvas_tools,
};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn agent() -> Author {
        Author::agent("snippet")
    }

    #[test]
    fn link_parse_and_format_all_variants() {
        let cases = [
            ("canvas:proj-1/hero", "proj-1", "hero", None, None),
            (
                "canvas:proj-1/hero/btn-1",
                "proj-1",
                "hero",
                Some("btn-1"),
                None,
            ),
            ("canvas:proj-1/hero@42", "proj-1", "hero", None, Some(42)),
            (
                "canvas:proj-1/hero/btn-1@7",
                "proj-1",
                "hero",
                Some("btn-1"),
                Some(7),
            ),
        ];
        for (raw, cid, fid, el, rev) in cases {
            let parsed = parse_canvas_ref(raw).expect("valid canvas ref");
            assert_eq!(parsed.canvas_id, cid);
            assert_eq!(parsed.frame_id, fid);
            assert_eq!(parsed.el_id.as_deref(), el);
            assert_eq!(parsed.rev, rev);
            assert_eq!(parsed.to_ref(), raw);
        }

        let md = format_markdown_link(
            "Hero [v2]",
            &CanvasLink::new("proj-1", "hero", Some("cta".into()), Some(9)),
        );
        assert_eq!(md, "[Hero  v2](canvas:proj-1/hero/cta@9)");

        let extracted =
            extract_markdown_links(&format!("Check {md} and [other](https://example.com)"));
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].0, "Hero  v2");
        assert_eq!(extracted[0].1.rev, Some(9));

        for bad in [
            "artifact:a/b@1",
            "canvas:only-one",
            "canvas:a/b/c/d",
            "canvas:../evil/frame",
            "canvas:a/b@abc",
            "canvas:a/b@",
            "canvas:/frame",
        ] {
            assert!(parse_canvas_ref(bad).is_err(), "expected error for {bad}");
        }
    }

    #[test]
    fn batch_atomicity_rolls_back_on_failure() {
        let dir = TempDir::new().unwrap();
        let mut store = CanvasStore::open(dir.path(), "proj").unwrap();

        let commit = store
            .apply_batch(
                "create hero".into(),
                vec![Op::CreateFrame {
                    id: Some("hero".into()),
                    title: "Hero".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 1440.0,
                    h: 900.0,
                    html: "<div>v1</div>".into(),
                }],
                agent(),
                None,
            )
            .unwrap();
        assert_eq!(commit.rev, 1);

        // Second batch succeeds on op 1 (mutating hero) and fails on op 2 (missing frame).
        let err = store.apply_batch(
            "broken batch".into(),
            vec![
                Op::SetHtml {
                    frame_id: "hero".into(),
                    html: "<div>should-not-persist</div>".into(),
                },
                Op::MoveFrame {
                    frame_id: "missing-frame".into(),
                    x: 10.0,
                    y: 20.0,
                    w: None,
                    h: None,
                    title: None,
                },
            ],
            agent(),
            None,
        );
        assert!(matches!(err, Err(CanvasError::FrameNotFound(_))));

        // In-memory state, rev, and on-disk replay are untouched.
        assert_eq!(store.rev(), 1);
        assert!(
            store.frame("hero").unwrap().html.contains("v1")
                && store.frame("hero").unwrap().html.starts_with("<div")
        );
        assert_eq!(store.ops_since(0).len(), 1);

        let reopened = CanvasStore::open(dir.path(), "proj").unwrap();
        assert_eq!(reopened.rev(), 1);
        assert!(
            reopened.frame("hero").unwrap().html.contains("v1")
                && reopened.frame("hero").unwrap().html.starts_with("<div")
        );
    }

    #[test]
    fn revert_and_revert_of_revert_preserve_history() {
        let dir = TempDir::new().unwrap();
        let mut store = CanvasStore::open(dir.path(), "proj").unwrap();

        let b1 = store
            .apply_batch(
                "create hero".into(),
                vec![Op::CreateFrame {
                    id: Some("hero".into()),
                    title: "Hero".into(),
                    x: 10.0,
                    y: 20.0,
                    w: 1200.0,
                    h: 800.0,
                    html: "<section>v1</section>".into(),
                }],
                agent(),
                None,
            )
            .unwrap();

        let b2 = store
            .apply_batch(
                "update hero".into(),
                vec![
                    Op::SetHtml {
                        frame_id: "hero".into(),
                        html: "<section>v2</section>".into(),
                    },
                    Op::MoveFrame {
                        frame_id: "hero".into(),
                        x: 100.0,
                        y: 200.0,
                        w: Some(1440.0),
                        h: Some(900.0),
                        title: Some("Hero Updated".into()),
                    },
                ],
                agent(),
                None,
            )
            .unwrap();

        // Revert b2 -> back to v1 geometry & html, appended as rev 3.
        let r1 = store.revert_batch(&b2.batch_id, agent(), None).unwrap();
        assert_eq!(r1.rev, 3);
        let hero = store.frame("hero").unwrap();
        assert!(hero.html.contains("v1") && hero.html.starts_with("<section"));
        assert_eq!(
            (hero.x, hero.y, hero.w, hero.h),
            (10.0, 20.0, 1200.0, 800.0)
        );
        assert_eq!(hero.title, "Hero");

        // Revert of revert -> back to v2 geometry & html, appended as rev 4.
        let r2 = store.revert_batch(&r1.batch_id, agent(), None).unwrap();
        assert_eq!(r2.rev, 4);
        let hero = store.frame("hero").unwrap();
        assert!(hero.html.contains("v2") && hero.html.starts_with("<section"));
        assert_eq!(
            (hero.x, hero.y, hero.w, hero.h),
            (100.0, 200.0, 1440.0, 900.0)
        );
        assert_eq!(hero.title, "Hero Updated");

        // Revert creation -> frame removed at rev 5; revert that -> restored at rev 6.
        let del = store.revert_batch(&b1.batch_id, agent(), None).unwrap();
        assert_eq!(del.rev, 5);
        assert!(store.frame("hero").is_none());

        let restore = store.revert_batch(&del.batch_id, agent(), None).unwrap();
        assert_eq!(restore.rev, 6);
        let hero = store.frame("hero").unwrap();
        assert!(hero.html.contains("v2") && hero.html.starts_with("<section"));

        // History was never rewritten: all 6 batches exist in order.
        assert_eq!(store.ops_since(0).len(), 6);
    }

    #[test]
    fn store_replay_determinism_across_snapshot_boundary() {
        let dir = TempDir::new().unwrap();
        let mut store = CanvasStore::open(dir.path(), "proj").unwrap();

        store
            .apply_batch(
                "seed".into(),
                vec![Op::CreateFrame {
                    id: Some("board".into()),
                    title: "Board".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 1440.0,
                    h: 900.0,
                    html: "<div>0</div>".into(),
                }],
                agent(),
                None,
            )
            .unwrap();

        // Cross SNAPSHOT_INTERVAL (50) and add 5 more ops after snapshot.
        for i in 1..=55 {
            store
                .apply_batch(
                    format!("step {i}"),
                    vec![
                        Op::SetHtml {
                            frame_id: "board".into(),
                            html: format!("<div>{i}</div>"),
                        },
                        Op::MoveFrame {
                            frame_id: "board".into(),
                            x: i as f32 * 5.0,
                            y: i as f32 * 2.0,
                            w: None,
                            h: None,
                            title: None,
                        },
                    ],
                    agent(),
                    None,
                )
                .unwrap();
        }
        assert_eq!(store.rev(), 56);
        assert!(
            dir.path()
                .join("proj/snapshots/rev-00000050.json")
                .is_file()
        );

        // Reopen using snapshot + tail ops.
        let from_snap = CanvasStore::open(dir.path(), "proj").unwrap();
        assert_eq!(from_snap.rev(), store.rev());
        assert_eq!(from_snap.frames(), store.frames());

        // Delete snapshots and reopen from raw ops.jsonl only — must match byte-for-byte.
        std::fs::remove_dir_all(dir.path().join("proj/snapshots")).unwrap();
        let from_log_only = CanvasStore::open(dir.path(), "proj").unwrap();
        assert_eq!(from_log_only.rev(), store.rev());
        assert_eq!(from_log_only.frames(), store.frames());
    }

    #[test]
    fn stub_renderer_and_cache_keying() {
        let dir = TempDir::new().unwrap();
        let svc = CanvasService::with_stub_renderer(dir.path().to_path_buf());
        {
            let store = svc.store("proj").unwrap();
            let mut s = store.lock().unwrap();
            s.apply_batch(
                "create".into(),
                vec![Op::CreateFrame {
                    id: Some("card".into()),
                    title: "Card".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 200.0,
                    h: 100.0,
                    html: "<div>hi</div>".into(),
                }],
                agent(),
                None,
            )
            .unwrap();
        }

        let (p1, png1, layout1) = svc.render_frame_cached("proj", "card", None, 0.5).unwrap();
        assert_eq!((layout1.width, layout1.height, layout1.rev), (100, 50, 1));
        assert!(png1.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));
        assert!(p1.is_file());

        // Second call hits cache; higher scale creates distinct cache file.
        let (p2, png2, layout2) = svc.render_frame_cached("proj", "card", None, 1.0).unwrap();
        assert_ne!(p1, p2);
        assert_eq!((layout2.width, layout2.height), (200, 100));
        assert!(png2.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));

        // Signed thumb verification roundtrip.
        let url = signed_thumb_path("secret", "proj", "card", 1, 1000, 60);
        let parts: Vec<&str> = url.split('/').collect();
        // /a/canvas/{exp}/{sig}/{canvas_id}/{frame_id}/{rev}/{scale}/thumb.png
        let exp: i64 = parts[3].parse().unwrap();
        let sig = parts[4];
        assert!(verify_signed_thumb(
            "secret", exp, sig, "proj", "card", 1, 1000
        ));
        assert!(!verify_signed_thumb(
            "wrong", exp, sig, "proj", "card", 1, 1000
        ));
    }

    #[test]
    fn canvas_read_cap_events_surface_and_link_passthrough() {
        // 1. Read output cap triggers truncation + narrowing hint.
        let dir = TempDir::new().unwrap();
        let mut store = CanvasStore::open(dir.path(), "proj").unwrap();
        let big_html = "<div>".to_string() + &"x".repeat(4000) + "</div>";
        store
            .apply_batch(
                "big".into(),
                vec![Op::CreateFrame {
                    id: Some("big".into()),
                    title: "Big".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 1440.0,
                    h: 900.0,
                    html: big_html,
                }],
                agent(),
                None,
            )
            .unwrap();
        let capped = tools::build_read_payload(&store, Default::default(), 600);
        assert_eq!(capped["truncated"], true);
        assert!(capped["hint"].as_str().unwrap().contains("frame_id"));

        // 2. Websocket device event carries exact canvas_rev payload.
        let mut rx = crate::session::subscribe_device_events();
        let commit = BatchCommit {
            canvas_id: "proj".into(),
            rev: 7,
            batch_id: "b-test".into(),
            label: "refine card".into(),
            frame_ids: vec!["big".into()],
            created_ids: vec![],
            links: vec!["[Big](canvas:proj/big@7)".into()],
        };
        emit_canvas_rev_event(&commit);
        let ev = rx.try_recv().expect("canvas_rev event emitted");
        assert_eq!(ev["type"], "canvas_rev");
        assert_eq!(ev["rev"], 7);
        assert_eq!(ev["batch_id"], "b-test");
        assert_eq!(ev["label"], "refine card");
        assert_eq!(ev["frame_ids"], serde_json::json!(["big"]));

        // 3. Surface lowering routes all four canvas actions properly.
        for (action, expected_tool) in [
            ("read", "canvas_read"),
            ("apply", "canvas_apply"),
            ("thumbnail", "canvas_thumbnail"),
            ("pins", "canvas_pins"),
        ] {
            let lowered = crate::surface::lower(
                "canvas",
                &serde_json::json!({"action": action, "op": "list"}),
            )
            .unwrap()
            .unwrap();
            assert_eq!(lowered.0, expected_tool);
            if action == "pins" {
                assert_eq!(lowered.1["action"], "list");
            }
        }

        // 4. Transcript/message sanitiser preserves canvas: markdown links untouched.
        let msg = "Updated [Hero](canvas:proj/hero/btn-1@3) ready for review.";
        assert_eq!(crate::sanitize::clean_user_text(msg).as_deref(), Some(msg));
    }

    #[test]
    fn dom_op_replay_determinism_across_multiple_frames_and_reopen() {
        let dir = TempDir::new().unwrap();
        let mut store = CanvasStore::open(dir.path(), "proj").unwrap();

        store
            .apply_batch(
                "create f1".into(),
                vec![Op::CreateFrame {
                    id: Some("f1".into()),
                    title: "F1".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 800.0,
                    h: 600.0,
                    html: "<div>one</div>".into(),
                }],
                agent(),
                None,
            )
            .unwrap();
        store
            .apply_batch(
                "create f2".into(),
                vec![Op::CreateFrame {
                    id: Some("f2".into()),
                    title: "F2".into(),
                    x: 900.0,
                    y: 0.0,
                    w: 800.0,
                    h: 600.0,
                    html: "<div>two</div>".into(),
                }],
                agent(),
                None,
            )
            .unwrap();

        let f1_root = html::element_ids(&store.frame("f1").unwrap().html)[0].clone();
        let ins = store
            .apply_batch(
                "insert child".into(),
                vec![Op::Dom {
                    frame_id: "f1".into(),
                    dom: html::DomOp::Insert {
                        parent_id: f1_root,
                        index: 0,
                        html: "<p>inserted</p>".into(),
                    },
                }],
                agent(),
                None,
            )
            .unwrap();
        let inserted_id = ins.created_ids[0].clone();
        let after_ins_html = store.frame("f1").unwrap().html;

        store
            .apply_batch(
                "patch inserted".into(),
                vec![Op::Dom {
                    frame_id: "f1".into(),
                    dom: html::DomOp::Patch {
                        el_id: inserted_id.clone(),
                        attrs: vec![("class".into(), Some("accent".into()))],
                        style: vec![("color".into(), Some("blue".into()))],
                        text: Some("patched".into()),
                    },
                }],
                agent(),
                None,
            )
            .unwrap();

        // Reopen from disk and check both final state and historical frame_at_rev(3).
        let reopened = CanvasStore::open(dir.path(), "proj").unwrap();
        assert_eq!(reopened.frames(), store.frames());
        assert_eq!(
            reopened.frame_at_rev("f1", 3).unwrap().unwrap().html,
            after_ins_html
        );
    }

    #[test]
    fn format_version_rejects_future_snapshot_and_ops() {
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("future");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("ops.jsonl"),
            r#"{"format_version":99,"rev":1,"batch_id":"b-1","author":{"kind":"user","id":"u"},"ts":1,"label":"x","ops":[]}"#,
        )
        .unwrap();
        let err = CanvasStore::open(dir.path(), "future").unwrap_err();
        assert!(matches!(
            err,
            CanvasError::UnsupportedFormatVersion { found: 99, .. }
        ));
    }

    #[test]
    fn concurrent_writers_have_monotonic_revs_and_consistent_replay() {
        let dir = TempDir::new().unwrap();
        let svc = std::sync::Arc::new(CanvasService::with_stub_renderer(dir.path().to_path_buf()));
        let mut handles = Vec::new();
        for t in 0..4 {
            let svc = svc.clone();
            handles.push(std::thread::spawn(move || {
                let mut revs = Vec::new();
                for i in 0..10 {
                    let store = svc.store("conc").unwrap();
                    let mut s = store.lock().unwrap();
                    let c = s
                        .apply_batch(
                            format!("t{t}-{i}"),
                            vec![Op::CreateFrame {
                                id: Some(format!("f-{t}-{i}")),
                                title: format!("F {t}-{i}"),
                                x: 0.0,
                                y: 0.0,
                                w: 200.0,
                                h: 100.0,
                                html: format!("<div>{t}-{i}</div>"),
                            }],
                            Author::agent("a"),
                            None,
                        )
                        .unwrap();
                    revs.push(c.rev);
                }
                revs
            }));
        }
        let mut all_revs: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        all_revs.sort();
        assert_eq!(all_revs, (1..=40).collect::<Vec<u64>>());

        let reopened = CanvasStore::open(dir.path(), "conc").unwrap();
        assert_eq!(reopened.rev(), 40);
        assert_eq!(reopened.frames().len(), 40);
    }

    #[tokio::test]
    async fn render_timeout_and_utf8_read_cap() {
        struct SlowRenderer;
        impl FrameRenderer for SlowRenderer {
            fn render(
                &self,
                _html: &str,
                _w: u32,
                _h: u32,
                _s: f32,
            ) -> Result<Rendered, RenderError> {
                std::thread::sleep(std::time::Duration::from_millis(250));
                StubRenderer.render("", 10, 10, 1.0)
            }
            fn hit_test(&self, _html: &str, _w: u32, _x: f32, _y: f32) -> Option<String> {
                None
            }
        }
        let dir = TempDir::new().unwrap();
        let svc = std::sync::Arc::new(CanvasService::new(
            dir.path().to_path_buf(),
            std::sync::Arc::new(SlowRenderer),
        ));
        {
            let store = svc.store("slow").unwrap();
            let mut s = store.lock().unwrap();
            s.apply_batch(
                "create".into(),
                vec![Op::CreateFrame {
                    id: Some("f".into()),
                    title: "F".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 100.0,
                    h: 100.0,
                    html: format!("<div>{}</div>", "🚀".repeat(500)),
                }],
                agent(),
                None,
            )
            .unwrap();
            // Verify UTF-8 byte cap never exceeds max_bytes on multi-byte HTML or oversized since_rev batch.
            let read_frame = tools::build_read_payload(
                &s,
                tools::CanvasReadArgs {
                    frame_id: Some("f".into()),
                    ..Default::default()
                },
                600,
            );
            assert_eq!(read_frame["truncated"], true);
            assert!(serde_json::to_string(&read_frame).unwrap().len() <= 600);

            let read_ops = tools::build_read_payload(
                &s,
                tools::CanvasReadArgs {
                    since_rev: Some(0),
                    ..Default::default()
                },
                600,
            );
            assert_eq!(read_ops["truncated"], true);
            assert!(serde_json::to_string(&read_ops).unwrap().len() <= 600);
        }
        let err = svc
            .render_frame_cached_async(
                "slow".into(),
                "f".into(),
                None,
                1.0,
                std::time::Duration::from_millis(30),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CanvasError::Render(RenderError::Timeout)));
    }
}
