use super::*;

pub const DEFAULT_TAILSCALE_PORT: u16 = 8443;
pub const FUNNEL_PORTS: [u16; 3] = [443, 8443, 10000];

pub(super) enum Running {
    Process(tokio::process::Child),
    Tailscale { bin: PathBuf, funnel: bool, port: u16 },
    Nothing,
}

impl Running {
    pub(super) async fn stop(self) {
        match self {
            Running::Process(mut child) => {
                let _ = child.start_kill();
            }
            // Only our root mount: a plain `off` drops every path on the port,
            // including other services the user put there.
            Running::Tailscale { bin, funnel, port } => {
                let _ = tokio::process::Command::new(&bin)
                    .args([
                        if funnel { "funnel" } else { "serve" },
                        &format!("--https={port}"),
                        "--set-path=/",
                        "off",
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
            }
            Running::Nothing => {}
        }
    }
}

pub(super) async fn open(tunnel: Tunnel, port: u16) -> Result<(String, Running), String> {
    let local = format!("http://127.0.0.1:{port}");
    match tunnel {
        Tunnel::Url(u) => Ok((u, Running::Nothing)),
        Tunnel::None => Ok((local, Running::Nothing)),
        Tunnel::Cloudflare => {
            let bin = find_cloudflared().ok_or(
                "cloudflared is not installed. Install it (`brew install cloudflared`, or https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) and try again.",
            )?;
            start_cloudflared_quick(&bin, port).await
        }
        Tunnel::Tailscale { funnel, port: ts_port } => {
            let bin = find_tailscale().ok_or(
                "Tailscale is not installed. Install it from https://tailscale.com/download, run `tailscale up`, and try again.",
            )?;
            let dns = tailscale_dns_name(&bin).await.ok_or_else(|| {
                "Tailscale is installed but not connected. Run `tailscale up` and try again."
                    .to_string()
            })?;
            start_tailscale(&bin, &dns, port, ts_port, funnel).await
        }
        Tunnel::Auto { ts_port } => {
            if let Some(bin) = find_tailscale()
                && let Some(dns) = tailscale_dns_name(&bin).await {
                    match start_tailscale(&bin, &dns, port, ts_port, false).await {
                        Ok(up) => return Ok(up),
                        Err(e) => println!("tailscale serve did not start: {e}"),
                    }
                }
            if let Some(bin) = find_cloudflared() {
                return start_cloudflared_quick(&bin, port).await;
            }
            println!("{}", setup_help(port));
            Ok((local, Running::Nothing))
        }
    }
}

pub fn setup_help(port: u16) -> String {
    format!(
        "No tunnel is set up, so the daemon is only reachable from this machine (http://127.0.0.1:{port}).
To reach it from your phone, set up one of these, then run `snippet serve --restart`:
  Tailscale    install it from https://tailscale.com/download on this machine and your phone,
               then run `tailscale up`. Private to your tailnet, and the address never changes.
  Cloudflare   install cloudflared (`brew install cloudflared`, or
               https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/).
               A public https link that changes on every restart.
Pick one explicitly with `snippet serve --tunnel tailscale|funnel|cloudflare`, or bring your own with --public-url."
    )
}

fn runs(bin: &std::path::Path, arg: &str) -> bool {
    std::process::Command::new(bin)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn first_working(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|bin| runs(bin, "version")).cloned()
}

pub(crate) fn find_cloudflared() -> Option<PathBuf> {
    first_working(&[
        PathBuf::from("cloudflared"),
        home_dir().join(".snippet").join("bin").join("cloudflared"),
        PathBuf::from("/opt/homebrew/bin/cloudflared"),
        PathBuf::from("/usr/local/bin/cloudflared"),
    ])
}

fn find_tailscale() -> Option<PathBuf> {
    first_working(&[
        PathBuf::from("tailscale"),
        PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale"),
        PathBuf::from("/opt/homebrew/bin/tailscale"),
        PathBuf::from("/usr/local/bin/tailscale"),
    ])
}

async fn tailscale_dns_name(bin: &std::path::Path) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(bin)
            .args(["status", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    if v.get("BackendState").and_then(|s| s.as_str()) != Some("Running") {
        return None;
    }
    let dns = v.pointer("/Self/DNSName")?.as_str()?.trim_end_matches('.');
    (!dns.is_empty()).then(|| dns.to_string())
}

async fn start_tailscale(
    bin: &std::path::Path,
    dns: &str,
    port: u16,
    ts_port: u16,
    funnel: bool,
) -> Result<(String, Running), String> {
    let verb = if funnel { "funnel" } else { "serve" };
    let target = format!("http://127.0.0.1:{port}");
    if let Some(status) = tailscale_serve_status(bin).await
        && let Some(taken) = serve_conflict(&status, dns, ts_port, &target, funnel)
    {
        return Err(format!(
            "{taken}. Pick a free port with `snippet serve --tailscale-port <port>` (see `tailscale serve status`)."
        ));
    }
    let run = tokio::process::Command::new(bin)
        .args([verb, "--bg", &format!("--https={ts_port}"), &target])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .map_err(|_| {
            format!(
                "`tailscale {verb}` is waiting on something it needs (usually HTTPS or Funnel switched on for your tailnet). Run `tailscale {verb} --bg --https={ts_port} {target}` once by hand to see the link."
            )
        })?
        .map_err(|e| format!("launch tailscale: {e}"))?;
    if !out.status.success() {
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let hint = if said.contains("Access denied") || said.contains("permission") {
            "\nOn Linux, let your user manage Tailscale once: `sudo tailscale set --operator=$USER`."
        } else {
            ""
        };
        return Err(format!("tailscale {verb}: {}{hint}", said.trim()));
    }
    let url = if ts_port == 443 {
        format!("https://{dns}")
    } else {
        format!("https://{dns}:{ts_port}")
    };
    Ok((
        url,
        Running::Tailscale {
            bin: bin.to_path_buf(),
            funnel,
            port: ts_port,
        },
    ))
}

async fn tailscale_serve_status(bin: &std::path::Path) -> Option<serde_json::Value> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(bin)
            .args(["serve", "status", "--json"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return Some(serde_json::json!({}));
    }
    serde_json::from_str(&text).ok()
}

/// Why `ts_port` is not ours to take, if another service already holds it.
fn serve_conflict(
    status: &serde_json::Value,
    dns: &str,
    ts_port: u16,
    target: &str,
    funnel: bool,
) -> Option<String> {
    let key = ts_port.to_string();
    let tcp = status.pointer(&format!("/TCP/{key}"))?;
    if tcp.get("TCPForward").is_some() || tcp.get("TerminateTLS").is_some() {
        return Some(format!("Tailscale port {ts_port} already forwards raw TCP for another service"));
    }
    let host = format!("{dns}:{ts_port}");
    let handlers = status
        .get("Web")
        .and_then(|w| w.get(&host))
        .and_then(|w| w.get("Handlers"))
        .and_then(serde_json::Value::as_object);
    let others: Vec<&String> = handlers
        .map(|h| h.keys().filter(|path| path.as_str() != "/").collect())
        .unwrap_or_default();
    if let Some(root) = handlers.and_then(|h| h.get("/")) {
        let ours = root.get("Proxy").and_then(serde_json::Value::as_str) == Some(target);
        if !ours {
            let what = root
                .get("Proxy")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("another handler");
            return Some(format!("Tailscale port {ts_port} already serves {what} at /"));
        }
    }
    let public = status
        .get("AllowFunnel")
        .and_then(|f| f.get(&host))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !others.is_empty() && funnel && !public {
        return Some(format!(
            "Tailscale port {ts_port} is shared with other tailnet-only paths, and Funnel would make them public"
        ));
    }
    if public && !funnel {
        return Some(format!(
            "Tailscale port {ts_port} is public through Funnel, so Snippet would be exposed to the internet"
        ));
    }
    None
}

/// Launch `cloudflared tunnel --url` and capture the printed `*.trycloudflare.com`
/// URL. cloudflared's output goes to a log FILE (not a parent pipe): if we piped it
/// and stopped reading, the pipe would fill and cloudflared would die on SIGPIPE,
/// killing the tunnel. The returned child must be kept alive for the tunnel to serve.
async fn start_cloudflared_quick(
    bin: &std::path::Path,
    port: u16,
) -> Result<(String, Running), String> {
    let _ = std::fs::create_dir_all(snippet_dir());
    let log = snippet_dir().join("cloudflared.log");
    let _ = std::fs::remove_file(&log);
    let out = std::fs::File::create(&log).map_err(|e| format!("cloudflared log: {e}"))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut child = tokio::process::Command::new(bin)
        .args([
            "tunnel",
            "--no-autoupdate",
            "--url",
            &format!("http://localhost:{port}"),
        ])
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .map_err(|e| format!("launch cloudflared: {e}"))?;

    for _ in 0..100 {
        if let Ok(content) = std::fs::read_to_string(&log)
            && let Some(url) = extract_trycloudflare_url(&content) {
                return Ok((url, Running::Process(child)));
            }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = child.start_kill();
    Err("timed out waiting for the cloudflared URL".to_string())
}

fn extract_trycloudflare_url(s: &str) -> Option<String> {
    for line in s.lines() {
        if let Some(i) = line.find("https://") {
            let url: String = line[i..]
                .chars()
                .take_while(|c| !c.is_whitespace())
                .collect();
            if url.contains("trycloudflare.com") {
                return Some(url.trim_end_matches(['.', ',']).to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const DNS: &str = "box.tail0.ts.net";
    const OURS: &str = "http://127.0.0.1:8787";

    fn web(port: u16, handlers: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "TCP": { port.to_string(): { "HTTPS": true } },
            "Web": { format!("{DNS}:{port}"): { "Handlers": handlers } },
        })
    }

    #[test]
    fn free_or_already_ours_is_not_a_conflict() {
        assert_eq!(serve_conflict(&serde_json::json!({}), DNS, 8443, OURS, false), None);
        let ours = web(8443, serde_json::json!({ "/": { "Proxy": OURS } }));
        assert_eq!(serve_conflict(&ours, DNS, 8443, OURS, false), None);
        let elsewhere = web(443, serde_json::json!({ "/": { "Proxy": "http://127.0.0.1:3000" } }));
        assert_eq!(serve_conflict(&elsewhere, DNS, 8443, OURS, false), None);
    }

    #[test]
    fn another_service_at_root_or_raw_tcp_is_a_conflict() {
        let taken = web(8443, serde_json::json!({ "/": { "Proxy": "http://127.0.0.1:3000" } }));
        assert!(serve_conflict(&taken, DNS, 8443, OURS, false).unwrap().contains("127.0.0.1:3000"));
        let tcp = serde_json::json!({ "TCP": { "8443": { "TCPForward": "127.0.0.1:22" } } });
        assert!(serve_conflict(&tcp, DNS, 8443, OURS, false).unwrap().contains("raw TCP"));
    }

    #[test]
    fn other_paths_can_share_the_port_unless_exposure_changes() {
        let shared = web(8443, serde_json::json!({ "/grafana": { "Proxy": "http://127.0.0.1:3000" } }));
        assert_eq!(serve_conflict(&shared, DNS, 8443, OURS, false), None);
        assert!(serve_conflict(&shared, DNS, 8443, OURS, true).unwrap().contains("public"));
        let mut public = shared.clone();
        public["AllowFunnel"] = serde_json::json!({ format!("{DNS}:8443"): true });
        assert!(serve_conflict(&public, DNS, 8443, OURS, false).unwrap().contains("Funnel"));
        assert_eq!(serve_conflict(&public, DNS, 8443, OURS, true), None);
    }

    #[tokio::test]
    #[ignore = "changes this machine's tailscale serve config on ports 8447 and 8448"]
    async fn live_tailscale_claims_free_ports_and_releases_only_its_root() {
        let bin = find_tailscale().expect("tailscale");
        let dns = tailscale_dns_name(&bin).await.expect("tailscale up");
        let ts = |args: &[&str]| std::process::Command::new(&bin).args(args).output().unwrap();
        ts(&["serve", "--bg", "--https=8448", "http://127.0.0.1:3999"]);
        ts(&["serve", "--bg", "--https=8447", "--set-path=/other", "http://127.0.0.1:3999"]);

        let refused = start_tailscale(&bin, &dns, 8787, 8448, false).await.err();
        let shared = start_tailscale(&bin, &dns, 8787, 8447, false).await;
        let after_start = tailscale_serve_status(&bin).await.unwrap();
        let url = match shared {
            Ok((url, running)) => {
                running.stop().await;
                Ok(url)
            }
            Err(e) => Err(e),
        };
        let after_stop = tailscale_serve_status(&bin).await.unwrap();
        ts(&["serve", "--https=8447", "off"]);
        ts(&["serve", "--https=8448", "off"]);

        assert!(refused.unwrap_or_default().contains("127.0.0.1:3999"));
        assert_eq!(url.unwrap(), format!("https://{dns}:8447"));
        let handlers = |s: &serde_json::Value| -> Vec<String> {
            s.pointer(&format!("/Web/{dns}:8447/Handlers"))
                .and_then(serde_json::Value::as_object)
                .map(|h| h.keys().cloned().collect())
                .unwrap_or_default()
        };
        assert_eq!(handlers(&after_start), ["/", "/other"]);
        assert_eq!(handlers(&after_stop), ["/other"]);
    }

    #[test]
    fn funnel_ports_and_port_zero_are_validated() {
        assert!(super::super::resolve_tunnel(Some("funnel"), false, None, Some(9000)).is_err());
        assert!(super::super::resolve_tunnel(Some("funnel"), false, None, Some(10000)).is_ok());
        assert!(super::super::resolve_tunnel(Some("tailscale"), false, None, Some(0)).is_err());
        assert!(matches!(
            super::super::resolve_tunnel(None, false, None, None),
            Ok(Tunnel::Auto { ts_port: 8443 })
        ));
    }
}
