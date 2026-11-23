use rquickjs::{Context, Ctx, Exception, Function, Promise, Runtime};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub struct Globals {
    pub surfaces: Vec<(String, Vec<String>)>,
    pub plain: Vec<String>,
}

pub struct Limits {
    pub timeout: Duration,
    pub memory_bytes: usize,
    pub max_log_chars: usize,
}

pub enum HostRequest {
    Call {
        name: String,
        args: Value,
        reply: mpsc::Sender<Value>,
    },
    Ask {
        question: String,
        data: Value,
        reply: mpsc::Sender<Option<Value>>,
    },
}

pub enum Outcome {
    Completed { value: Value, logs: Vec<String> },
    Paused { question: String, data: Value, logs: Vec<String> },
    Failed { message: String, logs: Vec<String> },
}

const PRELUDE_BASE: &str = r#"
const __fmt = (a) => typeof a === 'string' ? a : (() => { try { const s = JSON.stringify(a); return s === undefined ? String(a) : s; } catch (_) { return String(a); } })();
const __logger = (...a) => { __snippet_log(a.map(__fmt).join(' ')); };
globalThis.console = { log: __logger, info: __logger, warn: __logger, error: __logger };
globalThis.agent = {
  ask: async (question, data) => {
    const r = __snippet_ask(String(question), JSON.stringify(data === undefined ? null : data));
    if (r === null) { throw new Error("__snippet_pause__"); }
    return JSON.parse(r);
  },
};
"#;

struct LogBuf {
    lines: Vec<String>,
    chars: usize,
    max: usize,
    truncated: bool,
}

impl LogBuf {
    fn push(&mut self, line: String) {
        if self.truncated {
            return;
        }
        self.chars += line.chars().count();
        if self.chars > self.max {
            self.truncated = true;
            self.lines.push("... logs truncated".to_string());
        } else {
            self.lines.push(line);
        }
    }
}

struct PauseState {
    flag: AtomicBool,
    info: Mutex<Option<(String, Value)>>,
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

fn build_prelude(globals: &Globals) -> String {
    let mut src = String::from(PRELUDE_BASE);
    for (surface, actions) in &globals.surfaces {
        let s = js_str(surface);
        src.push_str(&format!("globalThis[{s}] = {{\n"));
        for action in actions {
            let a = js_str(action);
            src.push_str(&format!(
                "  [{a}]: async (args = {{}}) => JSON.parse(__snippet_call({s}, JSON.stringify({{...args, action: {a}}}))),\n"
            ));
        }
        src.push_str("};\n");
    }
    for name in &globals.plain {
        let n = js_str(name);
        src.push_str(&format!(
            "globalThis[{n}] = async (args = {{}}) => JSON.parse(__snippet_call({n}, JSON.stringify(args)));\n"
        ));
    }
    src
}

fn fmt_timeout(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

pub fn spawn(
    code: String,
    globals: Globals,
    limits: Limits,
) -> (UnboundedReceiver<HostRequest>, tokio::task::JoinHandle<Outcome>) {
    let (tx, rx) = unbounded_channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let logs = Arc::new(Mutex::new(LogBuf {
        lines: Vec::new(),
        chars: 0,
        max: limits.max_log_chars,
        truncated: false,
    }));
    let thread_logs = logs.clone();
    std::thread::spawn(move || {
        let outcome = run(code, globals, limits, tx, thread_logs.clone());
        let _ = done_tx.send(outcome);
    });
    let handle = tokio::spawn(async move {
        match done_rx.await {
            Ok(o) => o,
            Err(_) => failed_with(&logs, "sandbox thread terminated unexpectedly".into()),
        }
    });
    (rx, handle)
}

fn failed_with(logs: &Arc<Mutex<LogBuf>>, message: String) -> Outcome {
    Outcome::Failed {
        message,
        logs: take_logs(logs),
    }
}

fn take_logs(logs: &Arc<Mutex<LogBuf>>) -> Vec<String> {
    logs.lock().map(|l| l.lines.clone()).unwrap_or_default()
}

fn run(
    code: String,
    globals: Globals,
    limits: Limits,
    tx: UnboundedSender<HostRequest>,
    logs: Arc<Mutex<LogBuf>>,
) -> Outcome {
    let fail = |message: String| failed_with(&logs, message);
    let deadline = Instant::now() + limits.timeout;
    let timed_out = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(PauseState {
        flag: AtomicBool::new(false),
        info: Mutex::new(None),
    });

    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return fail(format!("sandbox init failed: {e}")),
    };
    rt.set_memory_limit(limits.memory_bytes);
    {
        let timed_out = timed_out.clone();
        let pause = pause.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            if pause.flag.load(Ordering::SeqCst) {
                return true;
            }
            if Instant::now() >= deadline {
                timed_out.store(true, Ordering::SeqCst);
                return true;
            }
            false
        })));
    }
    let ctx = match Context::full(&rt) {
        Ok(c) => c,
        Err(e) => return fail(format!("sandbox init failed: {e}")),
    };
    let prelude = build_prelude(&globals);
    let wrapped = format!(
        "(async () => {{ {code} \n}})().then((v) => {{ const s = JSON.stringify(v); return s === undefined ? 'null' : s; }})"
    );

    let result: Result<String, String> = ctx.with(|ctx| {
        let log_fn = {
            let logs = logs.clone();
            Function::new(ctx.clone(), move |line: String| {
                if let Ok(mut l) = logs.lock() {
                    l.push(line);
                }
            })
        }
        .map_err(|e| e.to_string())?;
        let call_fn = {
            let tx = tx.clone();
            let timed_out = timed_out.clone();
            Function::new(ctx.clone(), move |ctx: Ctx<'_>, name: String, args: String| {
                let args: Value = serde_json::from_str(&args)
                    .map_err(|e| Exception::throw_message(&ctx, &format!("invalid args: {e}")))?;
                let (rtx, rrx) = mpsc::channel();
                tx.send(HostRequest::Call { name, args, reply: rtx })
                    .map_err(|_| Exception::throw_message(&ctx, "host is unavailable"))?;
                let remaining = deadline.saturating_duration_since(Instant::now());
                match rrx.recv_timeout(remaining) {
                    Ok(v) => Ok(v.to_string()),
                    Err(RecvTimeoutError::Timeout) => {
                        timed_out.store(true, Ordering::SeqCst);
                        Err(Exception::throw_message(&ctx, "timed out"))
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        Err(Exception::throw_message(&ctx, "host dropped the call"))
                    }
                }
            })
        }
        .map_err(|e| e.to_string())?;
        let ask_fn = {
            let tx = tx.clone();
            let pause = pause.clone();
            let timed_out = timed_out.clone();
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>, question: String, data: String| -> rquickjs::Result<Option<String>> {
                    let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                    let (rtx, rrx) = mpsc::channel();
                    tx.send(HostRequest::Ask {
                        question: question.clone(),
                        data: data.clone(),
                        reply: rtx,
                    })
                    .map_err(|_| Exception::throw_message(&ctx, "host is unavailable"))?;
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    match rrx.recv_timeout(remaining) {
                        Ok(Some(v)) => Ok(Some(v.to_string())),
                        Ok(None) => {
                            if let Ok(mut i) = pause.info.lock() {
                                *i = Some((question, data));
                            }
                            pause.flag.store(true, Ordering::SeqCst);
                            Ok(None)
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            timed_out.store(true, Ordering::SeqCst);
                            Err(Exception::throw_message(&ctx, "timed out"))
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            Err(Exception::throw_message(&ctx, "host dropped the question"))
                        }
                    }
                },
            )
        }
        .map_err(|e| e.to_string())?;

        let g = ctx.globals();
        g.set("__snippet_log", log_fn).map_err(|e| e.to_string())?;
        g.set("__snippet_call", call_fn).map_err(|e| e.to_string())?;
        g.set("__snippet_ask", ask_fn).map_err(|e| e.to_string())?;

        let describe = |ctx: &Ctx<'_>, e: rquickjs::Error| -> String {
            if !matches!(e, rquickjs::Error::Exception) {
                return e.to_string();
            }
            let exc = ctx.catch();
            if let Some(ex) = exc.as_exception() {
                let msg = ex.message().unwrap_or_default();
                match ex.stack() {
                    Some(st) if !st.trim().is_empty() => format!("{msg}\n{st}"),
                    _ => msg,
                }
            } else if let Ok(Some(s)) = exc.get::<rquickjs::Coerced<String>>().map(Some) {
                s.0
            } else {
                "unknown error".to_string()
            }
        };

        ctx.eval::<(), _>(prelude.as_str()).map_err(|e| describe(&ctx, e))?;
        let promise: Promise = ctx.eval(wrapped).map_err(|e| describe(&ctx, e))?;
        promise.finish::<String>().map_err(|e| describe(&ctx, e))
    });

    if pause.flag.load(Ordering::SeqCst) {
        let (question, data) = pause
            .info
            .lock()
            .ok()
            .and_then(|mut i| i.take())
            .unwrap_or_default();
        return Outcome::Paused {
            question,
            data,
            logs: take_logs(&logs),
        };
    }
    if timed_out.load(Ordering::SeqCst) {
        return fail(format!("timed out after {}", fmt_timeout(limits.timeout)));
    }
    match result {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(value) => Outcome::Completed {
                value,
                logs: take_logs(&logs),
            },
            Err(e) => fail(format!("result is not valid JSON: {e}")),
        },
        Err(message) => fail(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits(ms: u64) -> Limits {
        Limits {
            timeout: Duration::from_millis(ms),
            memory_bytes: 64 * 1024 * 1024,
            max_log_chars: 200,
        }
    }

    fn globals() -> Globals {
        Globals {
            surfaces: vec![("files".into(), vec!["read".into()])],
            plain: vec!["echo".into()],
        }
    }

    #[tokio::test]
    async fn returns_computed_object() {
        let (_rx, h) = spawn(
            "const a = [1,2,3].map(x => x * 2); return { a, u: undefined };".into(),
            globals(),
            limits(5000),
        );
        match h.await.unwrap() {
            Outcome::Completed { value, .. } => assert_eq!(value, json!({"a": [2, 4, 6]})),
            _ => panic!("expected completed"),
        }
        let (_rx, h) = spawn("let x = 1;".into(), globals(), limits(5000));
        match h.await.unwrap() {
            Outcome::Completed { value, .. } => assert_eq!(value, Value::Null),
            _ => panic!("expected completed"),
        }
    }

    #[tokio::test]
    async fn calls_round_trip() {
        let (mut rx, h) = spawn(
            "const a = await files.read({path: '/x'}); const b = await echo({n: 1}); return [a, b];".into(),
            globals(),
            limits(5000),
        );
        while let Some(req) = rx.recv().await {
            if let HostRequest::Call { name, args, reply } = req {
                reply.send(json!({"name": name, "args": args})).unwrap();
            }
        }
        match h.await.unwrap() {
            Outcome::Completed { value, .. } => assert_eq!(
                value,
                json!([
                    {"name": "files", "args": {"path": "/x", "action": "read"}},
                    {"name": "echo", "args": {"n": 1}}
                ])
            ),
            _ => panic!("expected completed"),
        }
    }

    #[tokio::test]
    async fn ask_continues_or_pauses() {
        let (mut rx, h) = spawn(
            "return await agent.ask('ok?', {a: 1});".into(),
            globals(),
            limits(5000),
        );
        if let Some(HostRequest::Ask { question, data, reply }) = rx.recv().await {
            assert_eq!(question, "ok?");
            assert_eq!(data, json!({"a": 1}));
            reply.send(Some(json!("yes"))).unwrap();
        }
        match h.await.unwrap() {
            Outcome::Completed { value, .. } => assert_eq!(value, json!("yes")),
            _ => panic!("expected completed"),
        }

        let (mut rx, h) = spawn(
            "console.log('before'); try { await agent.ask('q', [1]); } catch (e) {} while (true) {} ".into(),
            globals(),
            limits(5000),
        );
        if let Some(HostRequest::Ask { reply, .. }) = rx.recv().await {
            reply.send(None).unwrap();
        }
        match h.await.unwrap() {
            Outcome::Paused { question, data, logs } => {
                assert_eq!(question, "q");
                assert_eq!(data, json!([1]));
                assert_eq!(logs, vec!["before"]);
            }
            _ => panic!("expected paused"),
        }
    }

    #[tokio::test]
    async fn thrown_error_fails() {
        let (_rx, h) = spawn("throw new Error('boom');".into(), globals(), limits(5000));
        match h.await.unwrap() {
            Outcome::Failed { message, .. } => assert!(message.contains("boom")),
            _ => panic!("expected failed"),
        }
    }

    #[tokio::test]
    async fn infinite_loop_times_out() {
        let (_rx, h) = spawn("while (true) {}".into(), globals(), limits(300));
        match h.await.unwrap() {
            Outcome::Failed { message, .. } => assert!(message.starts_with("timed out after"), "{message}"),
            _ => panic!("expected failed"),
        }
    }

    #[tokio::test]
    async fn logs_captured_and_truncated() {
        let (_rx, h) = spawn(
            "console.log('a', {b: 1}); console.warn('x'.repeat(300)); console.log('late');".into(),
            globals(),
            limits(5000),
        );
        match h.await.unwrap() {
            Outcome::Completed { logs, .. } => {
                assert_eq!(logs, vec!["a {\"b\":1}".to_string(), "... logs truncated".to_string()]);
            }
            _ => panic!("expected completed"),
        }
    }
}
