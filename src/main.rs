mod onvif;

use std::{env, net::Ipv6Addr};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use onvif::{
    models::{
        MoveRequest, OnvifPresetsResponse, OnvifProfilesResponse, PresetRequest, PresetsQuery,
        StopRequest,
    },
    service::{OnvifAuthMode, OnvifConfig, OnvifError, OnvifService},
};

#[derive(Clone)]
struct AppState {
    webrtc_url: String,
    onvif: OnvifService,
}

struct AppConfig {
    webrtc_url: String,
    onvif: OnvifConfig,
}

impl AppConfig {
    fn from_env() -> Result<Self, String> {
        let webrtc_url =
            env::var("WEBRTC_URL").map_err(|_| "WEBRTC_URL is required".to_string())?;
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
            other => {
                return Err(format!(
                    "ONVIF_AUTH_MODE must be one of basic|wsse, got: {other}"
                ));
            }
        };

        Ok(Self {
            webrtc_url,
            onvif: OnvifConfig::from_base_url(
                onvif_url,
                onvif_username,
                onvif_password,
                onvif_profile_token,
                onvif_auth_mode,
            ),
        })
    }
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    let cfg = AppConfig::from_env().unwrap_or_else(|e| {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    });

    let client = reqwest::Client::builder()
        .build()
        .expect("failed to create HTTP client");

    let app_state = AppState {
        webrtc_url: cfg.webrtc_url,
        onvif: OnvifService::new(cfg.onvif, client),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/onvif/profiles", get(onvif_profiles))
        .route("/api/onvif/presets", get(onvif_presets))
        .route("/api/onvif/move", post(onvif_move))
        .route("/api/onvif/stop", post(onvif_stop))
        .route("/api/onvif/goto-preset", post(onvif_goto_preset))
        .with_state(app_state);

    let addr = (Ipv6Addr::UNSPECIFIED, 5000);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");

    println!("Listening on http://{addr:?}");
    axum::serve(listener, app).await.expect("server error");
}

async fn index(State(state): State<AppState>) -> Html<String> {
    let configured = serde_json::to_string(&state.onvif.configured_profile_token())
        .unwrap_or_else(|_| "null".to_string());
    let url = serde_json::to_string(&state.webrtc_url).unwrap_or_else(|_| "null".to_string());

    let html = HTML_TEMPLATE
        .replace("__WEBRTC_URL__", &url)
        .replace("__CONFIGURED_PROFILE_TOKEN__", &configured);

    Html(html)
}

async fn onvif_profiles(
    State(state): State<AppState>,
) -> Result<Json<OnvifProfilesResponse>, (StatusCode, String)> {
    state
        .onvif
        .profiles()
        .await
        .map(Json)
        .map_err(map_onvif_error)
}

async fn onvif_presets(
    State(state): State<AppState>,
    Query(query): Query<PresetsQuery>,
) -> Result<Json<OnvifPresetsResponse>, (StatusCode, String)> {
    state
        .onvif
        .presets(query.profile_token.as_deref())
        .await
        .map(Json)
        .map_err(map_onvif_error)
}

async fn onvif_move(
    State(state): State<AppState>,
    Json(payload): Json<MoveRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    state
        .onvif
        .move_camera(payload)
        .await
        .map_err(map_onvif_error)
}

async fn onvif_stop(
    State(state): State<AppState>,
    Json(payload): Json<StopRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    state.onvif.stop(payload).await.map_err(map_onvif_error)
}

async fn onvif_goto_preset(
    State(state): State<AppState>,
    Json(payload): Json<PresetRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    state
        .onvif
        .goto_preset(payload)
        .await
        .map_err(map_onvif_error)
}

fn map_onvif_error(err: OnvifError) -> (StatusCode, String) {
    (err.status_code(), err.message())
}

const HTML_TEMPLATE: &str = include_str!("index.html");
