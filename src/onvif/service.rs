use axum::http::StatusCode;
use base64::Engine;
use rand::RngExt;
use sha1::{Digest, Sha1};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::onvif::models::{
    MoveRequest, OnvifPreset, OnvifPresetsResponse, OnvifProfile, OnvifProfilesResponse,
    PresetRequest, StopRequest,
};

#[derive(Clone)]
pub struct OnvifService {
    cfg: OnvifConfig,
    client: reqwest::Client,
}

#[derive(Clone)]
pub struct OnvifConfig {
    pub profile_token: Option<String>,
    auth_mode: OnvifAuthMode,
    username: String,
    password: String,
    media_url: String,
    ptz_url: String,
}

#[derive(Clone, Copy)]
pub enum OnvifAuthMode {
    Basic,
    Wsse,
}

pub enum OnvifError {
    BadRequest(String),
    Upstream(String),
}

impl OnvifError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }

    pub fn message(self) -> String {
        match self {
            Self::BadRequest(msg) | Self::Upstream(msg) => msg,
        }
    }
}

impl OnvifConfig {
    pub fn from_base_url(
        base_url: String,
        username: String,
        password: String,
        profile_token: Option<String>,
        auth_mode: OnvifAuthMode,
    ) -> Self {
        Self {
            profile_token,
            auth_mode,
            username,
            password,
            media_url: rewrite_onvif_device_to_service(&base_url),
            ptz_url: rewrite_onvif_service_to_device(&base_url),
        }
    }
}

impl OnvifService {
    pub fn new(cfg: OnvifConfig, client: reqwest::Client) -> Self {
        Self { cfg, client }
    }

    pub fn configured_profile_token(&self) -> Option<&str> {
        self.cfg.profile_token.as_deref()
    }

    pub async fn profiles(&self) -> Result<OnvifProfilesResponse, OnvifError> {
        let soap = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:trt="http://www.onvif.org/ver10/media/wsdl">
  <s:Body>
    <trt:GetProfiles/>
  </s:Body>
</s:Envelope>"#;

        let response = self.send_soap_raw_to(soap, &self.cfg.media_url).await?;
        let profiles = parse_profiles_response(&response)?;

        Ok(OnvifProfilesResponse {
            media_url: self.cfg.media_url.clone(),
            configured_profile_token: self.cfg.profile_token.clone(),
            profiles,
        })
    }

    pub async fn presets(
        &self,
        override_profile_token: Option<&str>,
    ) -> Result<OnvifPresetsResponse, OnvifError> {
        let profile = self.resolve_profile_token(override_profile_token)?;

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

        let response = self.send_soap_raw_to(&soap, &self.cfg.ptz_url).await?;
        let presets = parse_presets_response(&response)?;

        Ok(OnvifPresetsResponse {
            ptz_url: self.cfg.ptz_url.clone(),
            profile_token: profile,
            presets,
        })
    }

    pub async fn move_camera(&self, payload: MoveRequest) -> Result<&'static str, OnvifError> {
        let profile = self.resolve_profile_token(payload.profile_token.as_deref())?;
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

        self.send_soap_raw_to(&soap, &self.cfg.ptz_url).await?;
        Ok("move sent")
    }

    pub async fn stop(&self, payload: StopRequest) -> Result<&'static str, OnvifError> {
        let profile = self.resolve_profile_token(payload.profile_token.as_deref())?;
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

        self.send_soap_raw_to(&soap, &self.cfg.ptz_url).await?;
        Ok("stop sent")
    }

    pub async fn goto_preset(&self, payload: PresetRequest) -> Result<&'static str, OnvifError> {
        let profile = self.resolve_profile_token(payload.profile_token.as_deref())?;
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

        self.send_soap_raw_to(&soap, &self.cfg.ptz_url).await?;
        Ok("goto preset sent")
    }

    fn resolve_profile_token(&self, override_token: Option<&str>) -> Result<String, OnvifError> {
        override_token
            .filter(|v| !v.trim().is_empty())
            .map(ToString::to_string)
            .or_else(|| self.cfg.profile_token.clone())
            .ok_or_else(|| {
                OnvifError::BadRequest(
                    "ONVIF_PROFILE_TOKEN is not set. Use /api/onvif/profiles first.".to_string(),
                )
            })
    }

    async fn send_soap_raw_to(&self, body: &str, url: &str) -> Result<String, OnvifError> {
        self.send_once(body, self.cfg.auth_mode, url)
            .await
            .map_err(OnvifError::Upstream)
    }

    async fn send_once(
        &self,
        body: &str,
        mode: OnvifAuthMode,
        url: &str,
    ) -> Result<String, String> {
        let mut request = self
            .client
            .post(url)
            .header("Connection", "close")
            .header("Accept-Encoding", "identity")
            .header("Content-Type", "application/soap+xml; charset=utf-8");

        let final_body = match mode {
            OnvifAuthMode::Basic => {
                let auth = format!("{}:{}", self.cfg.username, self.cfg.password);
                let auth = base64::engine::general_purpose::STANDARD.encode(auth);
                request = request.header("Authorization", format!("Basic {auth}"));
                body.to_string()
            }
            OnvifAuthMode::Wsse => add_wsse_header(body, &self.cfg.username, &self.cfg.password)?,
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
}

fn add_wsse_header(envelope: &str, username: &str, password: &str) -> Result<String, String> {
    if !envelope.contains("<s:Body>") {
        return Err("soap envelope does not contain <s:Body>".to_string());
    }

    let created = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|e| format!("failed to format wsse created timestamp: {e}"))?;

    let mut nonce_raw = [0u8; 20];
    rand::rng().fill(&mut nonce_raw);
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

fn parse_profiles_response(xml: &str) -> Result<Vec<OnvifProfile>, OnvifError> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| OnvifError::Upstream(format!("failed to parse ONVIF profile XML: {e}")))?;

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

fn parse_presets_response(xml: &str) -> Result<Vec<OnvifPreset>, OnvifError> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| OnvifError::Upstream(format!("failed to parse ONVIF presets XML: {e}")))?;

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
