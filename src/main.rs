use std::{env, net::Ipv6Addr, sync::Arc};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    client: reqwest::Client,
}

#[derive(Clone)]
struct Config {
    webrtc_url: String,
    onvif_username: String,
    onvif_password: String,
    onvif_profile_token: Option<String>,
    onvif_auth_mode: OnvifAuthMode,
    onvif_media_url: String,
    onvif_ptz_url: String,
}

#[derive(Clone, Copy)]
enum OnvifAuthMode {
    Basic,
    Wsse,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let webrtc_url =
            env::var("WEBRTC_URL").map_err(|_| "WEBRTC_URL is required".to_string())?;
        let onvif_url = env::var("ONVIF_URL").map_err(|_| "ONVIF_URL is required".to_string())?;
        let onvif_username =
            env::var("ONVIF_USERNAME").map_err(|_| "ONVIF_USERNAME is required".to_string())?;
        let onvif_password =
            env::var("ONVIF_PASSWORD").map_err(|_| "ONVIF_PASSWORD is required".to_string())?;

        let onvif_auth_mode = match env::var("ONVIF_AUTH_MODE")
            .unwrap_or_else(|_| "wsse".to_string())
            .to_lowercase()
            .as_str()
        {
            "basic" => OnvifAuthMode::Basic,
            "wsse" => OnvifAuthMode::Wsse,
            other => {
                return Err(format!(
                    "ONVIF_AUTH_MODE must be one of basic|wsse, got: {other}"
                ));
            }
        };

        Ok(Self {
            webrtc_url,
            onvif_media_url: rewrite_onvif_device_to_service(&onvif_url),
            onvif_ptz_url: rewrite_onvif_service_to_device(&onvif_url),
            onvif_username,
            onvif_password,
            onvif_profile_token: None,
            onvif_auth_mode,
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

#[derive(Serialize)]
struct OnvifPreset {
    token: String,
    name: String,
}

#[derive(Serialize)]
struct OnvifPresetsResponse {
    ptz_url: String,
    profile_token: String,
    presets: Vec<OnvifPreset>,
}

#[derive(Deserialize)]
struct PresetsQuery {
    profile_token: Option<String>,
}

#[derive(Deserialize)]
struct MoveRequest {
    x: f32,
    y: f32,
    zoom: Option<f32>,
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

    let client = reqwest::Client::builder()
        .build()
        .expect("failed to create HTTP client");

    let app_state = AppState {
        cfg: Arc::new(cfg.clone()),
        client,
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/onvif/profiles", get(onvif_profiles))
        .route("/api/onvif/presets", get(onvif_presets))
        .route("/api/onvif/move", post(onvif_move))
        .route("/api/onvif/stop", post(onvif_stop))
        .route("/api/onvif/goto-preset", post(onvif_goto_preset))
        .with_state(app_state);

    let addr = (Ipv6Addr::UNSPECIFIED, 3000);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");

    println!("Listening on http://{addr:?}");
    axum::serve(listener, app).await.expect("server error");
}

async fn index(State(state): State<AppState>) -> Html<String> {
    let configured = serde_json::to_string(&state.cfg.onvif_profile_token)
        .unwrap_or_else(|_| "null".to_string());

    let url = serde_json::to_string(&state.cfg.webrtc_url).unwrap_or_else(|_| "null".to_string());

    let html = HTML_TEMPLATE
        .replace("__WEBRTC_URL__", &url)
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

    let response = send_onvif_soap_raw_to(&state, soap, &state.cfg.onvif_media_url).await?;
    let profiles = parse_profiles_response(&response)?;

    Ok(Json(OnvifProfilesResponse {
        media_url: state.cfg.onvif_media_url.clone(),
        configured_profile_token: state.cfg.onvif_profile_token.clone(),
        profiles,
    }))
}

async fn onvif_move(
    State(state): State<AppState>,
    Json(payload): Json<MoveRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let profile = profile_token_or_err(&state, payload.profile_token.as_deref())?;
    let zoom = payload.zoom.unwrap_or(0.0);

    let soap = format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <s:Body>
    <tptz:ContinuousMove>
      <tptz:ProfileToken>{profile}</tptz:ProfileToken>
      <tptz:Velocity>
        <tt:PanTilt x="{x}" y="{y}" />
        <tt:Zoom x="{zoom}" />
      </tptz:Velocity>
    </tptz:ContinuousMove>
  </s:Body>
</s:Envelope>"#,
        profile = profile,
        x = payload.x,
        y = payload.y,
        zoom = zoom,
    );

    send_onvif_soap_raw_to(&state, &soap, &state.cfg.onvif_ptz_url).await?;
    Ok("move sent")
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

    send_onvif_soap_raw_to(&state, &soap, &state.cfg.onvif_ptz_url).await?;
    Ok("stop sent")
}

async fn onvif_presets(
    State(state): State<AppState>,
    Query(query): Query<PresetsQuery>,
) -> Result<Json<OnvifPresetsResponse>, (StatusCode, String)> {
    let profile = profile_token_or_err(&state, query.profile_token.as_deref())?;
    let soap = format!(
        r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl">
  <s:Body>
    <tptz:GetPresets>
      <tptz:ProfileToken>{profile}</tptz:ProfileToken>
    </tptz:GetPresets>
  </s:Body>
</s:Envelope>"#,
        profile = profile,
    );

    let response = send_onvif_soap_raw_to(&state, &soap, &state.cfg.onvif_ptz_url).await?;
    let presets = parse_presets_response(&response)?;

    Ok(Json(OnvifPresetsResponse {
        ptz_url: state.cfg.onvif_ptz_url.clone(),
        profile_token: profile.to_string(),
        presets,
    }))
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

    send_onvif_soap_raw_to(&state, &soap, &state.cfg.onvif_ptz_url).await?;
    Ok("goto preset sent")
}

async fn send_onvif_soap_raw_to(
    state: &AppState,
    body: &str,
    url: &str,
) -> Result<String, (StatusCode, String)> {
    send_onvif_once(state, body, state.cfg.onvif_auth_mode, url)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))
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
        OnvifAuthMode::Basic => {
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

fn parse_presets_response(xml: &str) -> Result<Vec<OnvifPreset>, (StatusCode, String)> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to parse ONVIF presets XML: {e}"),
        )
    })?;

    let mut presets = Vec::new();
    for node in doc
        .descendants()
        .filter(|n| n.tag_name().name() == "Preset")
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
        presets.push(OnvifPreset { token, name });
    }

    Ok(presets)
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

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

const HTML_TEMPLATE: &str = include_str!("index.html");
