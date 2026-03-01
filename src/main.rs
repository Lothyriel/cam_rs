use std::{env, fs, net::SocketAddr, path::Path, sync::Arc};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::process::Command;
use tower_http::services::ServeDir;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    client: reqwest::Client,
}

#[derive(Clone)]
struct Config {
    bind_addr: String,
    rtsp_url: String,
    web_password: String,
    onvif_url: String,
    onvif_username: String,
    onvif_password: String,
    onvif_profile_token: Option<String>,
    onvif_auth_mode: OnvifAuthMode,
    onvif_media_url: String,
    onvif_ptz_url: String,
    hls_dir: String,
}

#[derive(Clone, Copy)]
enum OnvifAuthMode {
    Basic,
    Wsse,
    Auto,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".to_string());
        let rtsp_url = env::var("RTSP_URL").map_err(|_| "RTSP_URL is required".to_string())?;
        let web_password =
            env::var("WEB_PASSWORD").map_err(|_| "WEB_PASSWORD is required".to_string())?;
        let onvif_url = env::var("ONVIF_URL").map_err(|_| "ONVIF_URL is required".to_string())?;
        let onvif_username =
            env::var("ONVIF_USERNAME").map_err(|_| "ONVIF_USERNAME is required".to_string())?;
        let onvif_password =
            env::var("ONVIF_PASSWORD").map_err(|_| "ONVIF_PASSWORD is required".to_string())?;
        let onvif_profile_token = env::var("ONVIF_PROFILE_TOKEN").ok();
        let onvif_auth_mode = match env::var("ONVIF_AUTH_MODE")
            .unwrap_or_else(|_| "wsse".to_string())
            .to_lowercase()
            .as_str()
        {
            "basic" => OnvifAuthMode::Basic,
            "wsse" => OnvifAuthMode::Wsse,
            "auto" => OnvifAuthMode::Auto,
            other => {
                return Err(format!(
                    "ONVIF_AUTH_MODE must be one of basic|wsse|auto, got: {other}"
                ));
            }
        };
        let onvif_media_url = rewrite_onvif_device_to_service(&onvif_url);
        let onvif_ptz_url = rewrite_onvif_service_to_device(&onvif_url);
        let hls_dir = env::var("HLS_DIR").unwrap_or_else(|_| "hls".to_string());

        Ok(Self {
            bind_addr,
            rtsp_url,
            web_password,
            onvif_url,
            onvif_username,
            onvif_password,
            onvif_profile_token,
            onvif_auth_mode,
            onvif_media_url,
            onvif_ptz_url,
            hls_dir,
        })
    }
}

#[derive(Serialize)]
struct OnvifProfile {
    token: String,
    name: String,
}

#[derive(Serialize)]
struct OnvifProfilesResponse {
    media_url: String,
    configured_profile_token: Option<String>,
    profiles: Vec<OnvifProfile>,
}

#[derive(Deserialize)]
struct MoveRequest {
    x: f32,
    y: f32,
    zoom: Option<f32>,
    timeout_ms: Option<u64>,
    profile_token: Option<String>,
}

#[derive(Deserialize)]
struct StopRequest {
    profile_token: Option<String>,
}

#[derive(Deserialize)]
struct PresetRequest {
    preset_token: String,
    profile_token: Option<String>,
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    let cfg = Config::from_env().unwrap_or_else(|e| {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    });

    if let Err(e) = start_hls_pipeline(&cfg).await {
        eprintln!("Failed to start HLS pipeline: {e}");
        std::process::exit(1);
    }

    let client = reqwest::Client::builder()
        .build()
        .expect("failed to create HTTP client");

    let app_state = AppState {
        cfg: Arc::new(cfg.clone()),
        client,
    };

    let app = Router::new()
        .route("/", get(index))
        .nest_service("/hls", ServeDir::new(cfg.hls_dir.clone()))
        .route("/api/onvif/profiles", get(onvif_profiles))
        .route("/api/onvif/move", post(onvif_move))
        .route("/api/onvif/stop", post(onvif_stop))
        .route("/api/onvif/goto-preset", post(onvif_goto_preset))
        .with_state(app_state);

    let addr: SocketAddr = cfg
        .bind_addr
        .parse()
        .expect("BIND_ADDR must be in host:port format");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");

    println!("Listening on http://{addr}");
    axum::serve(listener, app).await.expect("server error");
}

async fn start_hls_pipeline(cfg: &Config) -> Result<(), String> {
    let hls_path = Path::new(&cfg.hls_dir);
    fs::create_dir_all(hls_path)
        .map_err(|e| format!("failed to create HLS dir {}: {e}", cfg.hls_dir))?;

    for entry in fs::read_dir(hls_path)
        .map_err(|e| format!("failed to read HLS dir {}: {e}", cfg.hls_dir))?
    {
        let entry = entry.map_err(|e| format!("failed to read HLS dir entry: {e}"))?;
        if entry
            .file_type()
            .map_err(|e| format!("failed to read file type: {e}"))?
            .is_file()
        {
            let p = entry.path();
            let ext = p.extension().and_then(|v| v.to_str()).unwrap_or_default();
            if matches!(ext, "m3u8" | "ts" | "m4s" | "tmp" | "mp4") {
                let _ = fs::remove_file(&p);
            }
        }
    }

    let playlist_path = hls_path.join("stream.m3u8");
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-fflags",
        "+genpts+discardcorrupt",
        "-use_wallclock_as_timestamps",
        "1",
        "-avoid_negative_ts",
        "make_zero",
        "-rtsp_transport",
        "tcp",
        "-i",
        &cfg.rtsp_url,
        "-an",
        "-c:v",
        "copy",
        "-f",
        "hls",
        "-hls_time",
        "1",
        "-hls_list_size",
        "6",
        "-hls_flags",
        "delete_segments+append_list+independent_segments+omit_endlist",
        "-hls_segment_type",
        "mpegts",
        playlist_path
            .to_str()
            .ok_or_else(|| "invalid HLS playlist path".to_string())?,
    ])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::inherit());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start ffmpeg: {e}"))?;

    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => eprintln!("HLS ffmpeg process exited: {status}"),
            Err(e) => eprintln!("Failed waiting for HLS ffmpeg process: {e}"),
        }
    });

    Ok(())
}

async fn index(State(state): State<AppState>) -> Html<String> {
    let pass =
        serde_json::to_string(&state.cfg.web_password).unwrap_or_else(|_| "\"\"".to_string());
    let configured = serde_json::to_string(&state.cfg.onvif_profile_token)
        .unwrap_or_else(|_| "null".to_string());

    let html = HTML_TEMPLATE
        .replace("__WEB_PASSWORD__", &pass)
        .replace("__CONFIGURED_PROFILE_TOKEN__", &configured);

    Html(html)
}

async fn onvif_profiles(
    State(state): State<AppState>,
) -> Result<Json<OnvifProfilesResponse>, (StatusCode, String)> {
    let soap = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:trt="http://www.onvif.org/ver10/media/wsdl">
  <s:Body>
    <trt:GetProfiles/>
  </s:Body>
</s:Envelope>"#;

    let candidates = media_url_candidates(&state).await;
    let mut last_err = String::new();
    let mut chosen_url = String::new();
    let mut profiles = Vec::new();

    for media_url in candidates {
        match send_onvif_soap_raw_to(&state, soap, &media_url).await {
            Ok(response) => match parse_profiles_response(&response) {
                Ok(p) => {
                    chosen_url = media_url;
                    profiles = p;
                    break;
                }
                Err((_, e)) => {
                    last_err = format!("parse failed via {media_url}: {e}");
                }
            },
            Err((_, e)) => {
                last_err = format!("request failed via {media_url}: {e}");
            }
        }
    }

    if chosen_url.is_empty() {
        return Err((StatusCode::BAD_GATEWAY, last_err));
    }

    Ok(Json(OnvifProfilesResponse {
        media_url: chosen_url,
        configured_profile_token: state.cfg.onvif_profile_token.clone(),
        profiles,
    }))
}

async fn onvif_move(
    State(state): State<AppState>,
    Json(payload): Json<MoveRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let profile = profile_token_or_err(&state, payload.profile_token.as_deref())?;
    let timeout = payload.timeout_ms.unwrap_or(400);
    let zoom = payload.zoom.unwrap_or(0.0);

    let continuous = format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Body>
    <tptz:ContinuousMove>
      <tptz:ProfileToken>{profile}</tptz:ProfileToken>
      <tptz:Velocity>
        <tt:PanTilt x="{x}" y="{y}" />
        <tt:Zoom x="{zoom}" />
      </tptz:Velocity>
      <tptz:Timeout>PT{secs}.{ms:03}S</tptz:Timeout>
    </tptz:ContinuousMove>
  </s:Body>
</s:Envelope>"#,
        profile = profile,
        x = payload.x,
        y = payload.y,
        zoom = zoom,
        secs = timeout / 1000,
        ms = timeout % 1000
    );

    match send_onvif_ptz_with_fallback(&state, continuous).await {
        Ok(()) => Ok("move sent"),
        Err((_, cont_err)) => {
            let relative = format!(
                r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Body>
    <tptz:RelativeMove>
      <tptz:ProfileToken>{profile}</tptz:ProfileToken>
      <tptz:Translation>
        <tt:PanTilt x="{x}" y="{y}" />
        <tt:Zoom x="{zoom}" />
      </tptz:Translation>
      <tptz:Speed>
        <tt:PanTilt x="0.5" y="0.5" />
        <tt:Zoom x="0.5" />
      </tptz:Speed>
    </tptz:RelativeMove>
  </s:Body>
</s:Envelope>"#,
                profile = profile,
                x = payload.x,
                y = payload.y,
                zoom = zoom,
            );
            send_onvif_ptz_with_fallback(&state, relative)
                .await
                .map_err(|(_, rel_err)| {
                    (
                        StatusCode::BAD_GATEWAY,
                        format!(
                            "ContinuousMove failed: {cont_err}; RelativeMove failed: {rel_err}"
                        ),
                    )
                })?;
            Ok("move sent (relative)")
        }
    }
}

async fn onvif_stop(
    State(state): State<AppState>,
    Json(payload): Json<StopRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let profile = profile_token_or_err(&state, payload.profile_token.as_deref())?;
    let soap = format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl">
  <s:Body>
    <tptz:Stop>
      <tptz:ProfileToken>{profile}</tptz:ProfileToken>
      <tptz:PanTilt>true</tptz:PanTilt>
      <tptz:Zoom>true</tptz:Zoom>
    </tptz:Stop>
  </s:Body>
</s:Envelope>"#,
        profile = profile,
    );
    send_onvif_ptz_with_fallback(&state, soap).await?;
    Ok("stop sent")
}

async fn onvif_goto_preset(
    State(state): State<AppState>,
    Json(payload): Json<PresetRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let profile = profile_token_or_err(&state, payload.profile_token.as_deref())?;
    let soap = format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl">
  <s:Body>
    <tptz:GotoPreset>
      <tptz:ProfileToken>{profile}</tptz:ProfileToken>
      <tptz:PresetToken>{preset}</tptz:PresetToken>
    </tptz:GotoPreset>
  </s:Body>
</s:Envelope>"#,
        profile = profile,
        preset = payload.preset_token,
    );
    send_onvif_ptz_with_fallback(&state, soap).await?;
    Ok("goto preset sent")
}

async fn send_onvif_ptz_with_fallback(
    state: &AppState,
    body: String,
) -> Result<(), (StatusCode, String)> {
    let mut candidates = Vec::new();
    if let Ok(ptz_url) = discover_ptz_url(state).await {
        candidates.push(ptz_url);
    }
    candidates.push(state.cfg.onvif_url.clone());
    candidates.push(rewrite_onvif_service_to_device(&state.cfg.onvif_url));
    candidates.sort();
    candidates.dedup();

    let mut errors = Vec::new();
    for url in candidates {
        match send_onvif_soap_raw_to(state, &body, &url).await {
            Ok(_) => return Ok(()),
            Err((_, err)) => errors.push(format!("{url} -> {err}")),
        }
    }

    Err((
        StatusCode::BAD_GATEWAY,
        format!("all PTZ endpoint attempts failed: {}", errors.join(" | ")),
    ))
}

async fn send_onvif_soap_raw_to(
    state: &AppState,
    body: &str,
    url: &str,
) -> Result<String, (StatusCode, String)> {
    let result = match state.cfg.onvif_auth_mode {
        OnvifAuthMode::Basic => send_onvif_once(state, body, OnvifAuthMode::Basic, url).await,
        OnvifAuthMode::Wsse => send_onvif_once(state, body, OnvifAuthMode::Wsse, url).await,
        OnvifAuthMode::Auto => {
            match send_onvif_once(state, body, OnvifAuthMode::Basic, url).await {
                Ok(text) => Ok(text),
                Err(basic_err) => {
                    match send_onvif_once(state, body, OnvifAuthMode::Wsse, url).await {
                        Ok(text) => Ok(text),
                        Err(wsse_err) => Err(format!(
                            "basic auth failed: {basic_err}; wsse auth failed: {wsse_err}"
                        )),
                    }
                }
            }
        }
    };

    result.map_err(|e| (StatusCode::BAD_GATEWAY, e))
}

async fn send_onvif_once(
    state: &AppState,
    body: &str,
    mode: OnvifAuthMode,
    url: &str,
) -> Result<String, String> {
    let mut request = state
        .client
        .post(url)
        .header("Connection", "close")
        .header("Accept-Encoding", "identity")
        .header("Content-Type", "application/soap+xml; charset=utf-8");

    let final_body = match mode {
        OnvifAuthMode::Basic | OnvifAuthMode::Auto => {
            let auth = format!("{}:{}", state.cfg.onvif_username, state.cfg.onvif_password);
            let auth = base64::engine::general_purpose::STANDARD.encode(auth);
            request = request.header("Authorization", format!("Basic {auth}"));
            body.to_string()
        }
        OnvifAuthMode::Wsse => {
            add_wsse_header(body, &state.cfg.onvif_username, &state.cfg.onvif_password)?
        }
    };

    let response = request
        .body(final_body)
        .send()
        .await
        .map_err(|e| format!("ONVIF request failed: {e}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "failed to read response body".to_string());

    if !status.is_success() {
        return Err(format!("ONVIF error {status}: {text}"));
    }

    Ok(text)
}

fn add_wsse_header(envelope: &str, username: &str, password: &str) -> Result<String, String> {
    if !envelope.contains("<s:Body>") {
        return Err("soap envelope does not contain <s:Body>".to_string());
    }

    let created = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| format!("failed to format wsse created timestamp: {e}"))?;

    let mut nonce_raw = [0u8; 20];
    rand::rngs::OsRng.fill_bytes(&mut nonce_raw);
    let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(nonce_raw);

    let mut hasher = Sha1::new();
    hasher.update(nonce_raw);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    let digest = hasher.finalize();
    let digest_b64 = base64::engine::general_purpose::STANDARD.encode(digest);

    let wsse_header = format!(
        r#"<s:Header><wsse:Security s:mustUnderstand="1" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"><wsse:UsernameToken><wsse:Username>{}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</wsse:Nonce><wsu:Created>{}</wsu:Created></wsse:UsernameToken></wsse:Security></s:Header>"#,
        xml_escape(username),
        digest_b64,
        nonce_b64,
        created
    );

    Ok(envelope.replacen("<s:Body>", &format!("{wsse_header}<s:Body>"), 1))
}

fn profile_token_or_err<'a>(
    state: &'a AppState,
    override_token: Option<&'a str>,
) -> Result<&'a str, (StatusCode, String)> {
    override_token
        .filter(|v| !v.trim().is_empty())
        .or(state.cfg.onvif_profile_token.as_deref())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "ONVIF_PROFILE_TOKEN is not set. Use /api/onvif/profiles first.".to_string(),
            )
        })
}

async fn discover_media_url(state: &AppState) -> Result<String, (StatusCode, String)> {
    if !state.cfg.onvif_media_url.is_empty() {
        return Ok(state.cfg.onvif_media_url.clone());
    }
    let response = get_capabilities_response(state).await?;
    Ok(parse_capability_xaddr(&response, "Media").unwrap_or_else(|| state.cfg.onvif_url.clone()))
}

async fn discover_ptz_url(state: &AppState) -> Result<String, (StatusCode, String)> {
    if !state.cfg.onvif_ptz_url.is_empty() {
        return Ok(state.cfg.onvif_ptz_url.clone());
    }
    let response = get_capabilities_response(state).await?;
    Ok(parse_capability_xaddr(&response, "PTZ")
        .map(|u| rewrite_onvif_service_to_device(&u))
        .unwrap_or_else(|| rewrite_onvif_service_to_device(&state.cfg.onvif_url)))
}

async fn get_capabilities_response(state: &AppState) -> Result<String, (StatusCode, String)> {
    let soap = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl">
  <s:Body>
    <tds:GetCapabilities>
      <tds:Category>All</tds:Category>
    </tds:GetCapabilities>
  </s:Body>
</s:Envelope>"#;
    send_onvif_soap_raw_to(state, soap, &state.cfg.onvif_url).await
}

fn parse_capability_xaddr(xml: &str, capability_name: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    for capability in doc
        .descendants()
        .filter(|n| n.tag_name().name() == capability_name)
    {
        if let Some(xaddr) = capability
            .descendants()
            .find(|n| n.tag_name().name() == "XAddr")
            .and_then(|n| n.text())
        {
            return Some(xaddr.trim().to_string());
        }
    }
    None
}

fn parse_profiles_response(xml: &str) -> Result<Vec<OnvifProfile>, (StatusCode, String)> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to parse ONVIF profile XML: {e}"),
        )
    })?;

    let mut profiles = Vec::new();
    for node in doc
        .descendants()
        .filter(|n| n.tag_name().name() == "Profiles")
    {
        let token = node.attribute("token").unwrap_or("").trim().to_string();
        if token.is_empty() {
            continue;
        }
        let name = node
            .descendants()
            .find(|n| n.tag_name().name() == "Name")
            .and_then(|n| n.text())
            .unwrap_or("")
            .trim()
            .to_string();
        profiles.push(OnvifProfile { token, name });
    }

    Ok(profiles)
}

fn rewrite_onvif_service_to_device(url: &str) -> String {
    if url.ends_with("/onvif/service") {
        return format!(
            "{}{}",
            url.trim_end_matches("/onvif/service"),
            "/onvif/device_service"
        );
    }
    url.to_string()
}

fn rewrite_onvif_device_to_service(url: &str) -> String {
    if url.ends_with("/onvif/device_service") {
        return format!(
            "{}{}",
            url.trim_end_matches("/onvif/device_service"),
            "/onvif/service"
        );
    }
    url.to_string()
}

async fn media_url_candidates(state: &AppState) -> Vec<String> {
    let mut out = Vec::new();
    out.push(state.cfg.onvif_media_url.clone());
    if let Ok(url) = discover_media_url(state).await {
        out.push(url);
    }
    out.push(rewrite_onvif_device_to_service(&state.cfg.onvif_url));
    out.push(state.cfg.onvif_url.clone());

    out.sort();
    out.dedup();
    out
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

const HTML_TEMPLATE: &str = include_str!("index.html");
