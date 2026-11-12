//! One coordinator for ambient interrupts and the resume that follows them.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use super::autonomy::{
    deliver_to_mission_control, interrupt_mission_control, mission_control_busy, mission_control_inputs,
};
use super::Shared;

const RESUME_CAP: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Trigger {
    Barge,
    LiveVerdict,
    FinalVerdict,
    Delivery,
}

impl Trigger {
    fn name(self) -> &'static str {
        match self {
            Trigger::Barge => "barge",
            Trigger::LiveVerdict => "live_verdict",
            Trigger::FinalVerdict => "final_verdict",
            Trigger::Delivery => "delivery",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)]
pub(super) struct Interrupted {
    pub spoke: bool,
    pub mission_control: bool,
    pub generation: u64,
    pub already: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ready {
    Wait,
    Fire,
    Abandon,
}

struct Inner {
    interrupted_gen: Option<u64>,
    resume_armed: Option<(u64, Instant)>,
    classifying: u32,
    #[allow(dead_code)]
    last_transition: Instant,
}

impl Inner {
    fn new(now: Instant) -> Self {
        Inner { interrupted_gen: None, resume_armed: None, classifying: 0, last_transition: now }
    }

    fn already_interrupted(&self, generation: u64) -> bool {
        self.interrupted_gen == Some(generation)
    }

    fn mark_interrupted(&mut self, generation: u64, now: Instant) -> bool {
        if self.interrupted_gen == Some(generation) {
            return false;
        }
        self.interrupted_gen = Some(generation);
        self.last_transition = now;
        true
    }

    fn arm(&mut self, generation: u64, now: Instant) {
        if self.resume_armed.is_some_and(|(g, _)| g > generation) {
            return;
        }
        if self.interrupted_gen.is_none_or(|g| g < generation) {
            self.interrupted_gen = Some(generation);
        }
        self.resume_armed = Some((generation, now));
        self.last_transition = now;
    }

    fn delivered(&mut self, generation: u64, now: Instant) {
        if self.resume_armed.is_some_and(|(g, _)| g <= generation) {
            self.resume_armed = None;
            self.last_transition = now;
        }
    }

    fn resume_ready(&self, generation: u64, now: Instant, quiet: bool) -> Ready {
        let Some((armed, at)) = self.resume_armed else { return Ready::Abandon };
        if armed != generation || self.interrupted_gen != Some(generation) {
            return Ready::Abandon;
        }
        if now.saturating_duration_since(at) >= RESUME_CAP {
            return Ready::Fire;
        }
        if self.classifying > 0 || !quiet {
            return Ready::Wait;
        }
        Ready::Fire
    }

    fn claim(&mut self, generation: u64, now: Instant) -> bool {
        if self.resume_armed.is_some_and(|(g, _)| g == generation) && self.interrupted_gen == Some(generation) {
            self.resume_armed = None;
            self.last_transition = now;
            true
        } else {
            false
        }
    }
}

static STATE: LazyLock<Mutex<Inner>> = LazyLock::new(|| Mutex::new(Inner::new(Instant::now())));

pub(super) struct ClassifyGuard(bool);

pub(super) fn classify_begin(device: &str) -> ClassifyGuard {
    let active = crate::ambient::lease_holder().is_none_or(|(h, _)| h == device);
    if active {
        STATE.lock().unwrap().classifying += 1;
    }
    ClassifyGuard(active)
}

impl Drop for ClassifyGuard {
    fn drop(&mut self) {
        if self.0 {
            let mut s = STATE.lock().unwrap();
            s.classifying = s.classifying.saturating_sub(1);
        }
    }
}

pub(super) async fn interrupt(d: &Shared, device: &str, trigger: Trigger) -> Interrupted {
    interrupt_since(d, device, trigger, mission_control_inputs()).await
}

/// `sent` is `mission_control_inputs()` from when the judged turn began, so a turn delivered meanwhile blocks the cut.
pub(super) async fn interrupt_since(d: &Shared, device: &str, trigger: Trigger, sent: u64) -> Interrupted {
    let generation = crate::ambient::run_generation();
    let delivery = trigger == Trigger::Delivery;
    let mut out = Interrupted { generation, ..Interrupted::default() };
    if !delivery && STATE.lock().unwrap().already_interrupted(generation) {
        out.already = true;
        return out;
    }
    // Delivery has already cut speech itself, and cuts a busy Mission Control without needing a conversation.
    out.spoke = if delivery { false } else { crate::speech::interrupt(device) };
    let busy = mission_control_busy() && (delivery || crate::ambient::conversation().is_some());
    if (out.spoke || busy) && (delivery || mission_control_inputs() == sent) && !crate::ambient::waiting_for_user() {
        out.mission_control = interrupt_mission_control(d).await;
        if !delivery {
            STATE.lock().unwrap().mark_interrupted(generation, Instant::now());
            arm_resume(d, generation, false);
        }
    }
    eprintln!(
        "[attention] interrupt trigger={} gen={} spoke={} mc={}",
        trigger.name(),
        generation,
        out.spoke,
        out.mission_control
    );
    out
}

pub(super) fn arm_resume(d: &Shared, generation: u64, needs_interruption: bool) {
    let sent = mission_control_inputs();
    STATE.lock().unwrap().arm(generation, Instant::now());
    let daemon = d.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;
            if mission_control_inputs() != sent {
                return;
            }
            let quiet = super::autonomy::heard_quiet();
            let ready = STATE.lock().unwrap().resume_ready(generation, Instant::now(), quiet);
            match ready {
                Ready::Wait => {}
                Ready::Abandon => return,
                Ready::Fire => break,
            }
        }
        if mission_control_busy() || (needs_interruption && !crate::speech::interruption_pending()) {
            return;
        }
        if super::autonomy::last_resume_matches_inputs() {
            let _ = crate::speech::take_interruption();
            return;
        }
        if !STATE.lock().unwrap().claim(generation, Instant::now()) {
            return;
        }
        eprintln!("[ambient-speech] no new turn followed the barge-in; resuming Mission Control");
        let heard = crate::speech::take_interruption()
            .map(|h| super::ambient_watch::interruption_attrs(&h))
            .unwrap_or_default();
        deliver_to_mission_control(
            &daemon,
            format!("<ambient_resume{heard}>A sound cut your speech off but no new request followed. Carry on without mentioning it: if your last reply was cut short, restate the sentence that was cut and finish what `unsaid` still holds, in a sentence or two, as whole sentences and never starting mid-phrase (`interrupted_reply` is what they heard); otherwise answer Chief's latest turn or continue what you were doing.</ambient_resume>"),
        )
        .await;
        super::autonomy::mark_resume_delivered();
    });
}

pub(super) fn delivered(generation: u64) {
    STATE.lock().unwrap().delivered(generation, Instant::now());
}

pub(crate) fn snapshot() -> serde_json::Value {
    let s = STATE.lock().unwrap();
    let now = Instant::now();
    serde_json::json!({
        "interrupted_gen": s.interrupted_gen,
        "resume_armed": s.resume_armed.map(|(g, _)| g),
        "resume_age_ms": s.resume_armed.map(|(_, at)| now.saturating_duration_since(at).as_millis() as u64),
        "classifying": s.classifying,
    })
}

pub(crate) fn state() -> &'static str {
    let armed = STATE.lock().unwrap().resume_armed.is_some();
    state_of(
        crate::ambient::state_name() != "off",
        crate::ambient::call_quiet() || crate::ambient::room_busy(),
        crate::ambient::floor_held(),
        crate::speech::pending(),
        armed || crate::ambient::waiting_for_user(),
        crate::ambient::conversation().is_some(),
    )
}

fn state_of(enabled: bool, quiet: bool, held: bool, speaking: bool, waiting: bool, engaged: bool) -> &'static str {
    if !enabled {
        "off"
    } else if quiet {
        "quiet"
    } else if held {
        "held"
    } else if speaking {
        "speaking"
    } else if waiting {
        "waiting"
    } else if engaged {
        "engaged"
    } else {
        "passive"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Clock(Instant);
    impl Clock {
        fn at(&self, millis: u64) -> Instant {
            self.0 + Duration::from_millis(millis)
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Step {
        Interrupt(u64, u64),
        Delivered(u64, u64),
        ClassifyBegin,
        ClassifyEnd,
    }

    #[test]
    fn inner_table() {
        struct Case {
            name: &'static str,
            steps: &'static [Step],
            check_gen: u64,
            check_ms: u64,
            quiet: bool,
            want_second_already: Option<bool>,
            want_ready: Ready,
            want_armed: Option<u64>,
        }

        let cases = [
            Case {
                name: "(no interrupt, delivered) is a no-op",
                steps: &[Step::Delivered(1, 1000)],
                check_gen: 1,
                check_ms: 5000,
                quiet: true,
                want_second_already: None,
                want_ready: Ready::Abandon,
                want_armed: None,
            },
            Case {
                name: "(interrupt gen 1, interrupt gen 1) second returns already",
                steps: &[Step::Interrupt(1, 0), Step::Interrupt(1, 500)],
                check_gen: 1,
                check_ms: 3000,
                quiet: true,
                want_second_already: Some(true),
                want_ready: Ready::Fire,
                want_armed: Some(1),
            },
            Case {
                name: "(interrupt gen 1, user turn gen 2, interrupt gen 2) fires again",
                steps: &[Step::Interrupt(1, 0), Step::Interrupt(2, 4000)],
                check_gen: 2,
                check_ms: 7000,
                quiet: true,
                want_second_already: Some(false),
                want_ready: Ready::Fire,
                want_armed: Some(2),
            },
            Case {
                name: "(interrupt gen 1, delivered gen 1) disarms the resume",
                steps: &[Step::Interrupt(1, 0), Step::Delivered(1, 2000)],
                check_gen: 1,
                check_ms: 3000,
                quiet: true,
                want_second_already: None,
                want_ready: Ready::Abandon,
                want_armed: None,
            },
            Case {
                name: "(interrupt gen 1, classify_begin, 3 s quiet) waits until guard drops",
                steps: &[Step::Interrupt(1, 0), Step::ClassifyBegin],
                check_gen: 1,
                check_ms: 3000,
                quiet: true,
                want_second_already: None,
                want_ready: Ready::Wait,
                want_armed: Some(1),
            },
            Case {
                name: "(interrupt gen 1, classify_begin then drop, 3 s quiet) fires after guard drops",
                steps: &[Step::Interrupt(1, 0), Step::ClassifyBegin, Step::ClassifyEnd],
                check_gen: 1,
                check_ms: 3000,
                quiet: true,
                want_second_already: None,
                want_ready: Ready::Fire,
                want_armed: Some(1),
            },
            Case {
                name: "(interrupt gen 1, 60 s) fires regardless of classifying (cap)",
                steps: &[Step::Interrupt(1, 0), Step::ClassifyBegin],
                check_gen: 1,
                check_ms: 60_000,
                quiet: false,
                want_second_already: None,
                want_ready: Ready::Fire,
                want_armed: Some(1),
            },
        ];

        for case in cases {
            let t0 = Instant::now();
            let c = Clock(t0);
            let mut inner = Inner::new(t0);
            let mut interrupt_count = 0;
            let mut second_already = None;
            for &step in case.steps {
                match step {
                    Step::Interrupt(g, ms) => {
                        interrupt_count += 1;
                        let already = inner.already_interrupted(g);
                        if !already {
                            inner.mark_interrupted(g, c.at(ms));
                            inner.arm(g, c.at(ms));
                        }
                        if interrupt_count == 2 {
                            second_already = Some(already);
                        }
                    }
                    Step::Delivered(g, ms) => inner.delivered(g, c.at(ms)),
                    Step::ClassifyBegin => inner.classifying += 1,
                    Step::ClassifyEnd => inner.classifying = inner.classifying.saturating_sub(1),
                }
            }
            assert_eq!(second_already, case.want_second_already, "{}", case.name);
            assert_eq!(inner.resume_armed.map(|(g, _)| g), case.want_armed, "{}", case.name);
            assert_eq!(inner.resume_ready(case.check_gen, c.at(case.check_ms), case.quiet), case.want_ready, "{}", case.name);
        }
    }

    #[test]
    fn state_precedence() {
        assert_eq!(state_of(false, true, true, true, true, true), "off");
        assert_eq!(state_of(true, true, true, true, true, true), "quiet");
        assert_eq!(state_of(true, false, true, true, true, true), "held");
        assert_eq!(state_of(true, false, false, true, true, true), "speaking");
        assert_eq!(state_of(true, false, false, false, true, true), "waiting");
        assert_eq!(state_of(true, false, false, false, false, true), "engaged");
        assert_eq!(state_of(true, false, false, false, false, false), "passive");
    }

    #[test]
    fn f5_slow_final_verdict_delivers_turn_without_ambient_resume() {
        let t0 = Instant::now();
        let c = Clock(t0);
        let mut inner = Inner::new(t0);
        let mut mc_deliveries: Vec<&'static str> = Vec::new();

        // t = 0 ms: user barges in on gen 1; final classifier guard is acquired for this turn.
        assert!(inner.mark_interrupted(1, c.at(0)));
        inner.arm(1, c.at(0));
        inner.classifying += 1;

        let poll_resume = |inner: &mut Inner, ms: u64, quiet: bool, out: &mut Vec<&'static str>| {
            if inner.resume_ready(1, c.at(ms), quiet) == Ready::Fire && inner.claim(1, c.at(ms)) {
                out.push("<ambient_resume>");
            }
        };

        // Poll every 500 ms up to 3.5 s: even after 2.5 s of quiet, classifying > 0 blocks <ambient_resume>.
        for step in 1..=7 {
            let ms = step * 500;
            let quiet = ms >= 2500;
            poll_resume(&mut inner, ms, quiet, &mut mc_deliveries);
        }
        assert!(mc_deliveries.is_empty());

        // t = 3500 ms: final verdict arrives; second interrupt check for gen 1 is a no-op,
        // user turn advances to gen 2 and delivers to Mission Control, then classify guard drops.
        assert!(inner.already_interrupted(1));
        mc_deliveries.push("<ambient_turn>");
        inner.delivered(2, c.at(3500));
        inner.classifying -= 1;

        // Polling past the 60 s cap produces no <ambient_resume>.
        for step in 8..=130 {
            poll_resume(&mut inner, step * 500, true, &mut mc_deliveries);
        }
        assert_eq!(mc_deliveries, vec!["<ambient_turn>"]);
    }

    #[tokio::test]
    async fn f5_end_to_end_slow_verdict_delivers_turn_once_without_ambient_resume() {
        use std::collections::HashMap;
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        let daemon: Shared = Arc::new(super::super::Daemon {
            config: Mutex::new(crate::config::SnippetConfig::default()),
            config_path: PathBuf::new(),
            token: String::new(),
            hostname: String::from("test"),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            shells: crate::term::SessionTerms::new(PathBuf::from(".")),
            git_write: tokio::sync::Mutex::new(()),
            seen_nonces: Mutex::new(HashMap::new()),
            queue_hidden: Mutex::new(HashMap::new()),
            queue_revision: AtomicU64::new(0),
            mission_control_root: tempfile::tempdir().unwrap().keep(),
            store: crate::store::Store::open_in_memory().unwrap(),
            coordination_events: tokio::sync::broadcast::channel(16).0,
            recurring_root: tempfile::tempdir().unwrap().keep(),
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let join = tokio::spawn(async {
            std::future::pending::<()>().await;
            Err("never".to_string())
        });
        let handle = crate::session::SessionHandle {
            input_tx: tx,
            join,
            state_path: PathBuf::new(),
            stream: None,
        };
        daemon
            .sessions
            .lock()
            .await
            .insert(crate::mission_control::SESSION_ID.to_string(), super::super::live_from_handle(handle, None));

        // Barge in on the current generation and hold a ClassifyGuard across the 2.5 s quiet window.
        let gen0 = crate::ambient::run_generation();
        let guard = classify_begin("test-device");
        arm_resume(&daemon, gen0, false);

        // Final verdict arrives at 3.5 s while the guard is still held.
        tokio::time::sleep(Duration::from_millis(3500)).await;
        crate::ambient::note_user_turn();
        deliver_to_mission_control(&daemon, "<ambient_turn>what is on my calendar</ambient_turn>".into()).await;
        drop(guard);

        // Give the background resume task more than one poll tick to observe the drop.
        tokio::time::sleep(Duration::from_millis(750)).await;

        let mut delivered = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let crate::harness::LoopInput::UserMessage(text) = msg {
                delivered.push(text);
            }
        }
        assert_eq!(delivered.len(), 1, "expected only the user turn, got: {delivered:?}");
        assert!(delivered[0].contains("<ambient_turn>"));
        assert!(!delivered[0].contains("<ambient_resume"));

        daemon.sessions.lock().await.clear();
    }
}
