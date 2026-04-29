use std::{
    collections::VecDeque,
    env,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    fs,
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{Mutex, RwLock, watch},
    task::JoinHandle,
    time::{MissedTickBehavior, interval, sleep},
};

use crate::onvif::{models::PresetRequest, service::OnvifService};

const MIN_PTZ_INTERVAL_MS: u64 = 50;
const MIN_ROLL_SECS: u64 = 1;
const STACK_CAPACITY_DIVISOR: usize = 32;
const DYNAMIC_THRESHOLD_ADJUSTMENT: u8 = 4;
const TRACK_LOCK_DURATION: Duration = Duration::from_millis(1500);
const TRACK_LOST_TIMEOUT: Duration = Duration::from_secs(1);
const TRACK_SMOOTHING_CURRENT: f32 = 0.55;
const TRACK_SMOOTHING_NEW: f32 = 0.45;
const PTZ_STOP_THRESHOLD: f32 = 0.01;
const PTZ_UPDATE_THRESHOLD: f32 = 0.02;
const RECORDING_CHUNK_SIZE: usize = 16 * 1024;
const DEFAULT_CAMERA_SETTLE_MS: u64 = 1200;
const DEFAULT_PRESET_SETTLE_MS: u64 = 3000;

#[derive(Clone)]
pub struct AiController {
    shared: Arc<AiShared>,
    workers: Arc<Mutex<Option<AiWorkers>>>,
    cfg: AiConfig,
    onvif: OnvifService,
    camera_settle: Duration,
    preset_settle: Duration,
}

struct AiShared {
    runtime: RwLock<AiRuntime>,
    events: watch::Sender<AiStateResponse>,
}

struct AiWorkers {
    shutdown: watch::Sender<bool>,
    detection: JoinHandle<()>,
    ptz: JoinHandle<()>,
    recording: JoinHandle<()>,
}

#[derive(Clone)]
pub struct AiConfig {
    enabled: bool,
    configured: bool,
    ffmpeg_bin: String,
    rtsp_url: String,
    detection_width: usize,
    detection_height: usize,
    detection_fps: u32,
    ema_alpha_milli: u32,
    threshold: u8,
    min_blob_percent: u8,
    motion_start_frames: u32,
    motion_end_frames: u32,
    ptz_interval: Duration,
    ptz_gain_x: f32,
    ptz_gain_y: f32,
    ptz_dead_zone: f32,
    ptz_rate_limit: f32,
    centered_hold: Duration,
    pre_roll: Duration,
    post_roll: Duration,
    camera_settle: Duration,
    preset_settle: Duration,
    recordings_dir: PathBuf,
    home_preset_token: Option<String>,
}

#[derive(Clone, Copy, Default, PartialEq, Serialize)]
pub struct AxisValue {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, PartialEq, Serialize)]
pub struct AiStateResponse {
    pub enabled: bool,
    pub configured: bool,
    pub available: bool,
    pub ai_active: bool,
    pub tracking: bool,
    pub recording: bool,
    pub manual_locked: bool,
    pub camera_moving: bool,
    pub target_offset: AxisValue,
    pub ptz_velocity: AxisValue,
    pub last_error: Option<String>,
}

#[derive(Clone, Default)]
struct AiRuntime {
    enabled: bool,
    configured: bool,
    available: bool,
    ai_active: bool,
    tracking: bool,
    recording: bool,
    manual_locked: bool,
    motion_active: bool,
    camera_moving: bool,
    camera_settle_until: Option<Instant>,
    target_offset: AxisValue,
    ptz_velocity: AxisValue,
    last_error: Option<String>,
    event_id: u64,
}

#[derive(Clone, Copy)]
struct Blob {
    centroid_x: f32,
    centroid_y: f32,
    area: usize,
}

#[derive(Clone, Copy)]
struct Track {
    offset: AxisValue,
    area: usize,
    locked_at: Instant,
    last_seen_at: Instant,
}

struct MotionDetector {
    width: usize,
    height: usize,
    alpha_milli: u32,
    base_threshold: u8,
    min_blob_area: usize,
    motion_start_frames: u32,
    motion_end_frames: u32,
    background_initialized: bool,
    background: Vec<u16>,
    mask: Vec<u8>,
    scratch: Vec<u8>,
    visited: Vec<u8>,
    stack: Vec<usize>,
    motion_frames: u32,
    empty_frames: u32,
    target: Option<Track>,
}

struct Chunk {
    at: Instant,
    data: Vec<u8>,
}

struct ClipBuffer {
    started_at: OffsetDateTime,
    post_roll_deadline: Option<Instant>,
    bytes: Vec<u8>,
}

impl AiConfig {
    pub fn from_env() -> Result<Self, String> {
        let enabled = parse_env_bool("AI_ENABLED", false)?;
        let ffmpeg_bin = env::var("FFMPEG_BIN").unwrap_or_else(|_| "ffmpeg".to_string());
        let detection_width = parse_env_usize("AI_DETECTION_WIDTH", 640)?;
        let detection_height = parse_env_usize("AI_DETECTION_HEIGHT", 360)?;
        let detection_fps = parse_env_u32("AI_DETECTION_FPS", 6)?;
        let ema_alpha_milli = parse_env_u32("AI_EMA_ALPHA_MILLI", 30)?;
        let threshold = parse_env_u8("AI_MOTION_THRESHOLD", 20)?;
        let min_blob_percent = parse_env_u8("AI_MIN_BLOB_PERCENT", 1)?;
        let motion_start_frames = parse_env_u32("AI_MOTION_START_FRAMES", 2)?;
        let motion_end_frames = parse_env_u32("AI_MOTION_END_FRAMES", 3)?;
        let ptz_interval_ms = parse_env_u64("AI_PTZ_INTERVAL_MS", 150)?;
        let ptz_gain_x = parse_env_f32("AI_PTZ_GAIN_X", 0.9)?;
        let ptz_gain_y = parse_env_f32("AI_PTZ_GAIN_Y", 0.9)?;
        let ptz_dead_zone = parse_env_f32("AI_PTZ_DEAD_ZONE", 0.1)?;
        let ptz_rate_limit = parse_env_f32("AI_PTZ_RATE_LIMIT", 0.2)?;
        let centered_hold_ms = parse_env_u64("AI_CENTERED_HOLD_MS", 500)?;
        let pre_roll_secs = parse_env_u64("AI_PRE_ROLL_SECS", 3)?;
        let post_roll_secs = parse_env_u64("AI_POST_ROLL_SECS", 7)?;
        let camera_settle_ms = parse_env_u64("AI_CAMERA_SETTLE_MS", DEFAULT_CAMERA_SETTLE_MS)?;
        let preset_settle_ms = parse_env_u64("AI_PRESET_SETTLE_MS", DEFAULT_PRESET_SETTLE_MS)?;
        let recordings_dir = env::var("AI_RECORDINGS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("recordings"));
        let home_preset_token = env::var("AI_HOME_PRESET_TOKEN").ok();
        let rtsp_url = env::var("AI_RTSP_URL")
            .or_else(|_| env::var("RTSP_URL"))
            .unwrap_or_default();
        let configured = !rtsp_url.trim().is_empty();
        if enabled && !configured {
            return Err("RTSP_URL or AI_RTSP_URL is required when AI_ENABLED=true".to_string());
        }

        Ok(Self {
            enabled,
            configured,
            ffmpeg_bin,
            rtsp_url,
            detection_width,
            detection_height,
            detection_fps,
            ema_alpha_milli,
            threshold,
            min_blob_percent,
            motion_start_frames,
            motion_end_frames,
            ptz_interval: Duration::from_millis(ptz_interval_ms.max(MIN_PTZ_INTERVAL_MS)),
            ptz_gain_x,
            ptz_gain_y,
            ptz_dead_zone,
            ptz_rate_limit,
            centered_hold: Duration::from_millis(centered_hold_ms),
            pre_roll: Duration::from_secs(pre_roll_secs.max(MIN_ROLL_SECS)),
            post_roll: Duration::from_secs(post_roll_secs.max(MIN_ROLL_SECS)),
            camera_settle: Duration::from_millis(camera_settle_ms),
            preset_settle: Duration::from_millis(preset_settle_ms),
            recordings_dir,
            home_preset_token,
        })
    }

    fn frame_len(&self) -> usize {
        self.detection_width * self.detection_height
    }
}

impl AiController {
    pub fn spawn(cfg: AiConfig, onvif: OnvifService) -> Self {
        let camera_settle = cfg.camera_settle;
        let preset_settle = cfg.preset_settle;
        let runtime = AiRuntime {
            enabled: cfg.enabled,
            configured: cfg.configured,
            ..AiRuntime::default()
        };
        let (events, _) = watch::channel(ai_state_response(&runtime));
        let shared = Arc::new(AiShared {
            runtime: RwLock::new(runtime),
            events,
        });

        let workers = if cfg.enabled && cfg.configured {
            Some(spawn_workers(cfg.clone(), onvif.clone(), Arc::clone(&shared)))
        } else {
            None
        };

        Self {
            shared,
            workers: Arc::new(Mutex::new(workers)),
            cfg,
            onvif,
            camera_settle,
            preset_settle,
        }
    }

    pub async fn snapshot(&self) -> AiStateResponse {
        let state = self.shared.runtime.read().await;
        ai_state_response(&state)
    }

    pub fn subscribe(&self) -> watch::Receiver<AiStateResponse> {
        self.shared.events.subscribe()
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        {
            let mut state = self.shared.runtime.write().await;
            if enabled && !state.configured {
                return Err("AI is not configured. Set RTSP_URL or AI_RTSP_URL first.".to_string());
            }

            state.enabled = enabled;
            if !enabled {
                state.available = false;
                state.camera_moving = false;
                state.camera_settle_until = None;
                state.recording = false;
                state.tracking = false;
                state.motion_active = false;
                state.target_offset = AxisValue::default();
                state.ptz_velocity = AxisValue::default();
                state.last_error = None;
            }
            refresh_runtime_flags(&mut state);
            publish_state(&state, &self.shared.events);
        }

        if enabled {
            self.ensure_workers_running().await;
        } else {
            self.stop_workers().await;
        }

        Ok(())
    }

    pub async fn note_camera_move_started(&self, velocity: AxisValue) {
        mark_camera_move_started(&self.shared, velocity).await;
    }

    pub async fn note_camera_move_stopped(&self) {
        mark_camera_move_stopped(&self.shared, self.camera_settle).await;
    }

    pub async fn note_camera_preset_move(&self) {
        mark_camera_motion_for(&self.shared, self.preset_settle).await;
    }

    async fn ensure_workers_running(&self) {
        if !self.cfg.configured {
            return;
        }

        let mut workers = self.workers.lock().await;
        if workers.is_none() {
            *workers = Some(spawn_workers(
                self.cfg.clone(),
                self.onvif.clone(),
                Arc::clone(&self.shared),
            ));
        }
    }

    async fn stop_workers(&self) {
        let workers = {
            let mut guard = self.workers.lock().await;
            guard.take()
        };

        if let Some(workers) = workers {
            workers.stop().await;
        }
    }
}

impl AiWorkers {
    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.detection.await;
        let _ = self.ptz.await;
        let _ = self.recording.await;
    }
}

fn spawn_workers(cfg: AiConfig, onvif: OnvifService, shared: Arc<AiShared>) -> AiWorkers {
    let (shutdown, detection_shutdown) = watch::channel(false);
    let ptz_shutdown = detection_shutdown.clone();
    let recording_shutdown = detection_shutdown.clone();

    let detection = {
        let detection_state = Arc::clone(&shared);
        let detection_cfg = cfg.clone();
        tokio::spawn(async move {
            run_detection_loop(detection_cfg, detection_state, detection_shutdown).await;
        })
    };

    let ptz = {
        let ptz_state = Arc::clone(&shared);
        let ptz_cfg = cfg.clone();
        let ptz_onvif = onvif.clone();
        tokio::spawn(async move {
            run_ptz_loop(ptz_cfg, ptz_onvif, ptz_state, ptz_shutdown).await;
        })
    };

    let recording = {
        let record_state = Arc::clone(&shared);
        tokio::spawn(async move {
            run_recording_loop(cfg, onvif, record_state, recording_shutdown).await;
        })
    };

    AiWorkers {
        shutdown,
        detection,
        ptz,
        recording,
    }
}

impl AiRuntime {
    fn camera_motion_active(&self) -> bool {
        self.camera_moving
            || self
                .camera_settle_until
                .map(|until| Instant::now() < until)
                .unwrap_or(false)
    }
}

impl MotionDetector {
    fn new(cfg: &AiConfig) -> Self {
        let frame_len = cfg.frame_len();
        let min_blob_area = (frame_len * cfg.min_blob_percent as usize / 100).max(1);
        Self {
            width: cfg.detection_width,
            height: cfg.detection_height,
            alpha_milli: cfg.ema_alpha_milli.clamp(1, 1000),
            base_threshold: cfg.threshold,
            min_blob_area,
            motion_start_frames: cfg.motion_start_frames.max(1),
            motion_end_frames: cfg.motion_end_frames.max(1),
            background_initialized: false,
            background: vec![0; frame_len],
            mask: vec![0; frame_len],
            scratch: vec![0; frame_len],
            visited: vec![0; frame_len],
            stack: Vec::with_capacity((frame_len / STACK_CAPACITY_DIVISOR).max(1)),
            motion_frames: 0,
            empty_frames: 0,
            target: None,
        }
    }

    fn process_frame(&mut self, frame: &[u8]) -> Option<AxisValue> {
        if !self.background_initialized {
            self.prime_background(frame);
        }

        let mut diff_sum = 0u64;
        for (idx, pixel) in frame.iter().copied().enumerate() {
            let current = (pixel as u16) << 8;
            let bg = self.background[idx];
            let updated =
                bg + (((current as i32 - bg as i32) * self.alpha_milli as i32) / 1000) as u16;
            self.background[idx] = updated;
            let diff = current.abs_diff(updated) >> 8;
            diff_sum += diff as u64;
            self.mask[idx] = 0;
        }

        let dynamic_threshold = self.base_threshold.max(
            ((diff_sum / frame.len() as u64) as u8).saturating_add(DYNAMIC_THRESHOLD_ADJUSTMENT),
        );

        for (idx, pixel) in frame.iter().copied().enumerate() {
            let bg = self.background[idx] >> 8;
            let diff = pixel.abs_diff(bg as u8);
            self.mask[idx] = u8::from(diff >= dynamic_threshold);
        }

        erode(self.width, self.height, &self.mask, &mut self.scratch);
        dilate(self.width, self.height, &self.scratch, &mut self.mask);

        let blob = self.largest_blob();
        if let Some(blob) = blob {
            self.empty_frames = 0;
            self.motion_frames = self.motion_frames.saturating_add(1);
            if self.motion_frames < self.motion_start_frames {
                return None;
            }
            return Some(self.update_track(blob));
        }

        self.motion_frames = 0;
        self.empty_frames = self.empty_frames.saturating_add(1);
        if self.empty_frames >= self.motion_end_frames {
            self.target = None;
        }
        None
    }

    fn motion_ended(&self, required_empty_frames: u32) -> bool {
        self.empty_frames >= required_empty_frames
    }

    fn reset_with_frame(&mut self, frame: &[u8]) {
        self.motion_frames = 0;
        self.empty_frames = 0;
        self.target = None;
        self.mask.fill(0);
        self.scratch.fill(0);
        self.visited.fill(0);
        self.prime_background(frame);
    }

    fn prime_background(&mut self, frame: &[u8]) {
        for (bg, px) in self.background.iter_mut().zip(frame.iter().copied()) {
            *bg = (px as u16) << 8;
        }
        self.background_initialized = true;
    }

    fn largest_blob(&mut self) -> Option<Blob> {
        self.visited.fill(0);
        let mut best = None;

        for idx in 0..self.mask.len() {
            if self.mask[idx] == 0 || self.visited[idx] == 1 {
                continue;
            }

            self.stack.clear();
            self.stack.push(idx);
            self.visited[idx] = 1;
            let mut area = 0usize;
            let mut sum_x = 0usize;
            let mut sum_y = 0usize;

            while let Some(current) = self.stack.pop() {
                let x = current % self.width;
                let y = current / self.width;
                area += 1;
                sum_x += x;
                sum_y += y;

                let min_x = x.saturating_sub(1);
                let max_x = (x + 1).min(self.width - 1);
                let min_y = y.saturating_sub(1);
                let max_y = (y + 1).min(self.height - 1);

                for ny in min_y..=max_y {
                    for nx in min_x..=max_x {
                        let next = ny * self.width + nx;
                        if self.mask[next] == 0 || self.visited[next] == 1 {
                            continue;
                        }
                        self.visited[next] = 1;
                        self.stack.push(next);
                    }
                }
            }

            if area < self.min_blob_area {
                continue;
            }

            let candidate = Blob {
                centroid_x: sum_x as f32 / area as f32,
                centroid_y: sum_y as f32 / area as f32,
                area,
            };

            if best
                .map(|current: Blob| candidate.area > current.area)
                .unwrap_or(true)
            {
                best = Some(candidate);
            }
        }

        best
    }

    fn update_track(&mut self, blob: Blob) -> AxisValue {
        let now = Instant::now();
        let raw_offset = AxisValue {
            x: ((blob.centroid_x - self.width as f32 / 2.0) / (self.width as f32 / 2.0))
                .clamp(-1.0, 1.0),
            y: ((blob.centroid_y - self.height as f32 / 2.0) / (self.height as f32 / 2.0))
                .clamp(-1.0, 1.0),
        };

        let should_replace = match self.target {
            Some(current) => {
                (now.duration_since(current.locked_at) >= TRACK_LOCK_DURATION
                    || blob.area > current.area.saturating_mul(2))
                    || now.duration_since(current.last_seen_at) > TRACK_LOST_TIMEOUT
            }
            None => true,
        };

        let next = if should_replace {
            Track {
                offset: raw_offset,
                area: blob.area,
                locked_at: now,
                last_seen_at: now,
            }
        } else {
            let current = self.target.unwrap();
            Track {
                offset: AxisValue {
                    x: current.offset.x * TRACK_SMOOTHING_CURRENT
                        + raw_offset.x * TRACK_SMOOTHING_NEW,
                    y: current.offset.y * TRACK_SMOOTHING_CURRENT
                        + raw_offset.y * TRACK_SMOOTHING_NEW,
                },
                area: blob.area,
                locked_at: current.locked_at,
                last_seen_at: now,
            }
        };

        self.target = Some(next);
        next.offset
    }
}

async fn run_detection_loop(cfg: AiConfig, state: Arc<AiShared>, mut shutdown: watch::Receiver<bool>) {
    let mut backoff = Duration::from_secs(1);

    loop {
        if *shutdown.borrow() {
            break;
        }

        match run_detection_session(&cfg, Arc::clone(&state), shutdown.clone()).await {
            Ok(true) => break,
            Ok(false) => backoff = Duration::from_secs(1),
            Err(err) => {
                update_error(&state, err).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
        set_available(&state, false).await;

        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            _ = sleep(backoff) => {}
        }
    }

    set_available(&state, false).await;
}

async fn run_detection_session(
    cfg: &AiConfig,
    state: Arc<AiShared>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<bool, String> {
    let filter = format!(
        "fps={},scale={}x{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2,format=gray",
        cfg.detection_fps,
        cfg.detection_width,
        cfg.detection_height,
        cfg.detection_width,
        cfg.detection_height
    );

    let mut child = Command::new(&cfg.ffmpeg_bin)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-hwaccel",
            "vaapi",
            "-rtsp_transport",
            "tcp",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-i",
            &cfg.rtsp_url,
            "-vf",
            &filter,
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start FFmpeg detection pipeline: {e}"))?;

    set_available(&state, true).await;
    clear_error(&state).await;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "FFmpeg detection stdout was not piped".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "FFmpeg detection stderr was not piped".to_string())?;

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    let mut detector = MotionDetector::new(cfg);
    let mut frame = vec![0u8; cfg.frame_len()];
    let mut motion_active = false;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    stop_child(&mut child).await;
                    let _ = stderr_task.await;
                    return Ok(true);
                }
            }
            read = stdout.read_exact(&mut frame) => match read {
            Ok(_) => {
                let suppress_detection = {
                    let runtime = state.runtime.read().await;
                    !runtime.enabled || runtime.camera_motion_active()
                };

                if suppress_detection {
                    detector.reset_with_frame(&frame);
                    clear_detection_state(&state).await;
                    continue;
                }

                if let Some(offset) = detector.process_frame(&frame) {
                    let mut runtime = state.runtime.write().await;
                    if !motion_active && detector.motion_frames >= cfg.motion_start_frames {
                        motion_active = true;
                        runtime.event_id = runtime.event_id.saturating_add(1);
                    }
                    runtime.available = true;
                    runtime.motion_active = motion_active;
                    runtime.tracking = motion_active;
                    runtime.target_offset = offset;
                    refresh_runtime_flags(&mut runtime);
                    publish_state(&runtime, &state.events);
                } else if motion_active && detector.motion_ended(cfg.motion_end_frames) {
                    let mut runtime = state.runtime.write().await;
                    motion_active = false;
                    runtime.motion_active = false;
                    runtime.tracking = false;
                    runtime.target_offset = AxisValue::default();
                    refresh_runtime_flags(&mut runtime);
                    publish_state(&runtime, &state.events);
                }
            }
            Err(err) => {
                stop_child(&mut child).await;
                let stderr = stderr_task.await.unwrap_or_default();
                let detail = if stderr.is_empty() {
                    format!("FFmpeg detection stream ended: {err}")
                } else {
                    format!("FFmpeg detection stream ended: {err}; {stderr}")
                };
                return Err(detail);
            }
            }
        }
    }
}

async fn run_ptz_loop(
    cfg: AiConfig,
    onvif: OnvifService,
    state: Arc<AiShared>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(cfg.ptz_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_sent = AxisValue::default();
    let mut centered_since = None;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
            _ = ticker.tick() => {}
        }

        let snapshot = {
            let runtime = state.runtime.read().await;
            runtime.clone()
        };

        if !snapshot.tracking {
            centered_since = None;
            if last_sent.x.abs() > PTZ_STOP_THRESHOLD || last_sent.y.abs() > PTZ_STOP_THRESHOLD {
                if let Err(err) = onvif
                    .stop(crate::onvif::models::StopRequest {
                        profile_token: None,
                    })
                    .await
                {
                    update_error(&state, err.message()).await;
                } else {
                    mark_camera_move_stopped(&state, cfg.camera_settle).await;
                }
                last_sent = AxisValue::default();
                set_ptz_velocity(&state, last_sent).await;
            }
            continue;
        }

        let offset = snapshot.target_offset;
        let centered = offset.x.abs() < cfg.ptz_dead_zone && offset.y.abs() < cfg.ptz_dead_zone;
        if centered {
            centered_since.get_or_insert_with(Instant::now);
        } else {
            centered_since = None;
        }

        let mut desired = AxisValue {
            x: if offset.x.abs() < cfg.ptz_dead_zone {
                0.0
            } else {
                (offset.x * cfg.ptz_gain_x).clamp(-1.0, 1.0)
            },
            y: if offset.y.abs() < cfg.ptz_dead_zone {
                0.0
            } else {
                (offset.y * cfg.ptz_gain_y).clamp(-1.0, 1.0)
            },
        };

        if centered_since
            .map(|at| at.elapsed() >= cfg.centered_hold)
            .unwrap_or(false)
        {
            desired = AxisValue::default();
        }

        let next = AxisValue {
            x: rate_limit(last_sent.x, desired.x, cfg.ptz_rate_limit),
            y: rate_limit(last_sent.y, desired.y, cfg.ptz_rate_limit),
        };

        if (next.x - last_sent.x).abs() < PTZ_UPDATE_THRESHOLD
            && (next.y - last_sent.y).abs() < PTZ_UPDATE_THRESHOLD
        {
            continue;
        }

        let result = if next.x.abs() < PTZ_STOP_THRESHOLD && next.y.abs() < PTZ_STOP_THRESHOLD {
            onvif
                .stop(crate::onvif::models::StopRequest {
                    profile_token: None,
                })
                .await
                .map(|_| "stop")
        } else {
            onvif
                .move_camera(crate::onvif::models::MoveRequest {
                    x: next.x,
                    y: next.y,
                    zoom: Some(0.0),
                    profile_token: None,
                })
                .await
                .map(|_| "move")
        };

        match result {
            Ok(_) => {
                last_sent = next;
                clear_error(&state).await;
                set_ptz_velocity(&state, next).await;
                if next.x.abs() < PTZ_STOP_THRESHOLD && next.y.abs() < PTZ_STOP_THRESHOLD {
                    mark_camera_move_stopped(&state, cfg.camera_settle).await;
                } else {
                    mark_camera_move_started(&state, next).await;
                }
            }
            Err(err) => {
                update_error(&state, err.message()).await;
            }
        }
    }

    if last_sent.x.abs() > PTZ_STOP_THRESHOLD || last_sent.y.abs() > PTZ_STOP_THRESHOLD {
        match onvif
            .stop(crate::onvif::models::StopRequest {
                profile_token: None,
            })
            .await
        {
            Ok(_) => clear_error(&state).await,
            Err(err) => update_error(&state, err.message()).await,
        }
    }
}

async fn run_recording_loop(
    cfg: AiConfig,
    onvif: OnvifService,
    state: Arc<AiShared>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);

    loop {
        if *shutdown.borrow() {
            break;
        }

        match run_recording_session(&cfg, &onvif, Arc::clone(&state), shutdown.clone()).await {
            Ok(true) => break,
            Ok(false) => backoff = Duration::from_secs(1),
            Err(err) => {
                update_error(&state, err).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }

        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            _ = sleep(backoff) => {}
        }
    }

    set_recording_state(&state, false).await;
}

async fn run_recording_session(
    cfg: &AiConfig,
    onvif: &OnvifService,
    state: Arc<AiShared>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<bool, String> {
    let mut child = Command::new(&cfg.ffmpeg_bin)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-rtsp_transport",
            "tcp",
            "-i",
            &cfg.rtsp_url,
            "-c",
            "copy",
            "-f",
            "mpegts",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start FFmpeg recording pipeline: {e}"))?;

    clear_error(&state).await;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "FFmpeg recording stdout was not piped".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "FFmpeg recording stderr was not piped".to_string())?;

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    let mut pre_roll = VecDeque::new();
    let mut clip: Option<ClipBuffer> = None;
    let mut active_event_id = 0u64;
    let mut buf = vec![0u8; RECORDING_CHUNK_SIZE];

    loop {
        let read = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    stop_child(&mut child).await;
                    let _ = stderr_task.await;
                    return Ok(true);
                }
                continue;
            }
            read = stdout.read(&mut buf) => read
                .map_err(|e| format!("failed to read FFmpeg recording stream: {e}"))?,
        };
        if read == 0 {
            stop_child(&mut child).await;
            let stderr = stderr_task.await.unwrap_or_default();
            if stderr.is_empty() {
                return Err("FFmpeg recording stream ended unexpectedly".to_string());
            }
            return Err(format!(
                "FFmpeg recording stream ended unexpectedly: {stderr}"
            ));
        }

        let chunk = buf[..read].to_vec();
        let now = Instant::now();
        pre_roll.push_back(Chunk {
            at: now,
            data: chunk.clone(),
        });
        while pre_roll
            .front()
            .map(|front| now.duration_since(front.at) > cfg.pre_roll)
            .unwrap_or(false)
        {
            pre_roll.pop_front();
        }

        let (motion_active, event_id) = {
            let runtime = state.runtime.read().await;
            if !runtime.enabled {
                (false, 0)
            } else {
                (runtime.motion_active, runtime.event_id)
            }
        };

        if event_id == 0 {
            if clip.take().is_some() {
                set_recording_state(&state, false).await;
            }
            active_event_id = 0;
            continue;
        }

        if motion_active && clip.is_none() && event_id != active_event_id {
            let mut bytes = Vec::new();
            for chunk in &pre_roll {
                bytes.extend_from_slice(&chunk.data);
            }
            clip = Some(ClipBuffer {
                started_at: OffsetDateTime::now_utc(),
                post_roll_deadline: None,
                bytes,
            });
            active_event_id = event_id;
            set_recording_state(&state, true).await;
        }

        if let Some(active_clip) = clip.as_mut() {
            active_clip.bytes.extend_from_slice(&chunk);
            if motion_active {
                active_clip.post_roll_deadline = None;
            } else {
                active_clip
                    .post_roll_deadline
                    .get_or_insert(now + cfg.post_roll);
            }

            if active_clip
                .post_roll_deadline
                .map(|deadline| now >= deadline)
                .unwrap_or(false)
            {
                let finalized = clip.take().unwrap();
                let path = recording_path(&cfg.recordings_dir, finalized.started_at)
                    .map_err(|e| format!("failed to build recording path: {e}"))?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("failed to create recordings directory: {e}"))?;
                }
                fs::write(&path, finalized.bytes)
                    .await
                    .map_err(|e| format!("failed to write recording {}: {e}", path.display()))?;
                set_recording_state(&state, false).await;
                return_home(cfg, onvif, &state).await;
            }
        }
    }
}

async fn return_home(cfg: &AiConfig, onvif: &OnvifService, state: &Arc<AiShared>) {
    set_ptz_velocity(state, AxisValue::default()).await;
    if let Some(preset_token) = cfg.home_preset_token.clone() {
        mark_camera_motion_for(state, cfg.preset_settle).await;
        match onvif
            .goto_preset(PresetRequest {
                preset_token,
                profile_token: None,
            })
            .await
        {
            Ok(_) => clear_error(state).await,
            Err(err) => update_error(state, err.message()).await,
        }
    }

    let mut runtime = state.runtime.write().await;
    runtime.camera_moving = false;
    runtime.tracking = false;
    runtime.motion_active = false;
    runtime.target_offset = AxisValue::default();
    refresh_runtime_flags(&mut runtime);
    publish_state(&runtime, &state.events);
}

async fn stop_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn recording_path(root: &PathBuf, started_at: OffsetDateTime) -> Result<PathBuf, String> {
    let stamp = started_at
        .format(&Rfc3339)
        .map_err(|e| e.to_string())?
        .replace(':', "-");
    Ok(root.join(format!("motion-{stamp}.ts")))
}

fn erode(width: usize, height: usize, input: &[u8], output: &mut [u8]) {
    output.fill(0);
    if width < 3 || height < 3 {
        return;
    }

    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let idx = y * width + x;
            let mut keep = 1u8;
            'outer: for ny in y - 1..=y + 1 {
                for nx in x - 1..=x + 1 {
                    if input[ny * width + nx] == 0 {
                        keep = 0;
                        break 'outer;
                    }
                }
            }
            output[idx] = keep;
        }
    }
}

fn dilate(width: usize, height: usize, input: &[u8], output: &mut [u8]) {
    output.fill(0);
    if width < 3 || height < 3 {
        return;
    }

    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let idx = y * width + x;
            let mut set = 0u8;
            'outer: for ny in y - 1..=y + 1 {
                for nx in x - 1..=x + 1 {
                    if input[ny * width + nx] == 1 {
                        set = 1;
                        break 'outer;
                    }
                }
            }
            output[idx] = set;
        }
    }
}

fn parse_env_bool(name: &str, default: bool) -> Result<bool, String> {
    match env::var(name) {
        Ok(value) => match value.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(format!(
                "{name} must be one of true|false|1|0|yes|no|on|off, got: {other}"
            )),
        },
        Err(_) => Ok(default),
    }
}

fn parse_env_usize(name: &str, default: usize) -> Result<usize, String> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| format!("{name} must be a positive integer"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn parse_env_u8(name: &str, default: u8) -> Result<u8, String> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<u8>()
                .map_err(|_| format!("{name} must be an integer between 0 and 255"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn parse_env_u32(name: &str, default: u32) -> Result<u32, String> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| format!("{name} must be a positive integer"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64, String> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| format!("{name} must be a positive integer"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn parse_env_f32(name: &str, default: f32) -> Result<f32, String> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<f32>()
                .map_err(|_| format!("{name} must be a number"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn rate_limit(current: f32, desired: f32, max_step: f32) -> f32 {
    let delta = (desired - current).clamp(-max_step, max_step);
    (current + delta).clamp(-1.0, 1.0)
}

async fn set_available(state: &Arc<AiShared>, available: bool) {
    let mut runtime = state.runtime.write().await;
    runtime.available = available;
    publish_state(&runtime, &state.events);
}

async fn set_ptz_velocity(state: &Arc<AiShared>, velocity: AxisValue) {
    let mut runtime = state.runtime.write().await;
    runtime.ptz_velocity = velocity;
    publish_state(&runtime, &state.events);
}

async fn set_recording_state(state: &Arc<AiShared>, recording: bool) {
    let mut runtime = state.runtime.write().await;
    runtime.recording = runtime.enabled && recording;
    refresh_runtime_flags(&mut runtime);
    publish_state(&runtime, &state.events);
}

async fn clear_error(state: &Arc<AiShared>) {
    let mut runtime = state.runtime.write().await;
    runtime.last_error = None;
    publish_state(&runtime, &state.events);
}

async fn update_error(state: &Arc<AiShared>, message: String) {
    let mut runtime = state.runtime.write().await;
    runtime.last_error = Some(message);
    publish_state(&runtime, &state.events);
}

async fn clear_detection_state(state: &Arc<AiShared>) {
    let mut runtime = state.runtime.write().await;
    runtime.motion_active = false;
    runtime.tracking = false;
    runtime.target_offset = AxisValue::default();
    refresh_runtime_flags(&mut runtime);
    publish_state(&runtime, &state.events);
}

async fn mark_camera_move_started(state: &Arc<AiShared>, velocity: AxisValue) {
    let mut runtime = state.runtime.write().await;
    runtime.camera_moving = true;
    runtime.camera_settle_until = None;
    runtime.ptz_velocity = velocity;
    runtime.tracking = false;
    runtime.target_offset = AxisValue::default();
    refresh_runtime_flags(&mut runtime);
    publish_state(&runtime, &state.events);
}

async fn mark_camera_move_stopped(state: &Arc<AiShared>, settle: Duration) {
    let mut runtime = state.runtime.write().await;
    runtime.camera_moving = false;
    runtime.camera_settle_until = Some(Instant::now() + settle);
    runtime.ptz_velocity = AxisValue::default();
    runtime.tracking = false;
    runtime.target_offset = AxisValue::default();
    refresh_runtime_flags(&mut runtime);
    publish_state(&runtime, &state.events);
}

async fn mark_camera_motion_for(state: &Arc<AiShared>, duration: Duration) {
    let mut runtime = state.runtime.write().await;
    runtime.camera_moving = false;
    runtime.camera_settle_until = Some(Instant::now() + duration);
    runtime.tracking = false;
    runtime.target_offset = AxisValue::default();
    refresh_runtime_flags(&mut runtime);
    publish_state(&runtime, &state.events);
}

fn ai_state_response(state: &AiRuntime) -> AiStateResponse {
    let camera_moving = state.camera_motion_active();
    AiStateResponse {
        enabled: state.enabled,
        configured: state.configured,
        available: state.available,
        ai_active: state.ai_active,
        tracking: state.tracking,
        recording: state.recording,
        manual_locked: state.enabled && (state.ai_active || camera_moving),
        camera_moving,
        target_offset: state.target_offset,
        ptz_velocity: state.ptz_velocity,
        last_error: state.last_error.clone(),
    }
}

fn publish_state(state: &AiRuntime, events: &watch::Sender<AiStateResponse>) {
    let _ = events.send(ai_state_response(state));
}

fn refresh_runtime_flags(runtime: &mut AiRuntime) {
    if !runtime.enabled {
        runtime.ai_active = false;
        runtime.manual_locked = false;
        return;
    }

    runtime.ai_active = runtime.motion_active || runtime.recording;
    runtime.manual_locked = runtime.ai_active || runtime.camera_motion_active();
}
