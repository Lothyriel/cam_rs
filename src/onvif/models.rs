use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct OnvifProfile {
    pub token: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct OnvifProfilesResponse {
    pub media_url: String,
    pub configured_profile_token: Option<String>,
    pub profiles: Vec<OnvifProfile>,
}

#[derive(Serialize)]
pub struct OnvifPreset {
    pub token: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct OnvifPresetsResponse {
    pub ptz_url: String,
    pub profile_token: String,
    pub presets: Vec<OnvifPreset>,
}

#[derive(Deserialize)]
pub struct PresetsQuery {
    pub profile_token: Option<String>,
}

#[derive(Deserialize)]
pub struct MoveRequest {
    pub x: f32,
    pub y: f32,
    pub zoom: Option<f32>,
    pub profile_token: Option<String>,
}

#[derive(Deserialize)]
pub struct StopRequest {
    pub profile_token: Option<String>,
}

#[derive(Deserialize)]
pub struct PresetRequest {
    pub preset_token: String,
    pub profile_token: Option<String>,
}
