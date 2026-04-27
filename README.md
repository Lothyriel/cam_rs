# cam_rs

Rust API + web UI for:
- live camera video via WebRTC (through go2rtc)
- ONVIF PTZ control (move, stop, presets)
- optional motion-driven PTZ auto-tracking with event recording

## Requirements

- Rust (for local run)
- Podman or Docker (optional, for container run)
- A camera with:
  - RTSP stream URL
  - ONVIF endpoint + credentials
- go2rtc running and reachable by browser

## Environment

Copy and edit:

```bash
cp .env.example .env
```

Variables:

- `WEBRTC_URL`  
  Browser URL for go2rtc stream page, example:  
  `https://rtc.your-domain/stream.html?src=cam`
- `ONVIF_URL`  
  Example: `http://10.0.0.11:2020/onvif/device_service`
- `ONVIF_USERNAME`
- `ONVIF_PASSWORD`
- `ONVIF_AUTH_MODE` (optional): `wsse` (default) or `basic`
- `ONVIF_PROFILE_TOKEN` (optional)
- `AI_ENABLED` (optional): `true` to enable motion detection + auto-tracking
- `AI_RTSP_URL` (optional): override RTSP source used by AI; falls back to `RTSP_URL`
- `AI_HOME_PRESET_TOKEN` (optional): preset token used after post-roll completes
- `AI_RECORDINGS_DIR` (optional): output directory for saved `.ts` motion clips
- `AI_CAMERA_SETTLE_MS` (optional): motion-detector cooldown after PTZ stop, defaults to `1200`
- `AI_PRESET_SETTLE_MS` (optional): longer cooldown after preset jumps, defaults to `3000`
- `FFMPEG_BIN` (optional): ffmpeg binary name/path, defaults to `ffmpeg`

When `AI_ENABLED=true`, the Rust API also uses `RTSP_URL` directly for:
- low-FPS grayscale motion detection
- RTSP copy-based event recording with 3s RAM pre-roll

## Run go2rtc

`go2rtc.yaml` is already wired to `${RTSP_URL}`:

```yaml
api:
  listen: ":1984"

streams:
  cam:
    - ${RTSP_URL}
```

Example run:

```bash
podman run -d \
  --name go2rtc \
  --restart unless-stopped \
  --env-file .env \
  -p 1984:1984 \
  -p 8555:8555/udp \
  -p 8555:8555/tcp \
  -v "$(pwd)/go2rtc.yaml:/config/go2rtc.yaml:ro,Z" \
  ghcr.io/alexxit/go2rtc:latest
```

## Run Rust API (local)

```bash
cargo run
```

Server listens on `:3000`.

## Run Rust API (container)

Build:

```bash
podman build -t cam_rs:latest .
```

Run:

```bash
podman run --replace \
  --name cam_rs \
  --env-file .env \
  -p 3000:3000 \
  cam_rs:latest
```

## API routes

- `GET /` UI
- `GET /api/onvif/profiles`
- `GET /api/onvif/presets?profile_token=...`
- `GET /api/ai/state`
- `POST /api/ai/enabled`
- `POST /api/onvif/move`
- `POST /api/onvif/stop`
- `POST /api/onvif/goto-preset`

## PTZ behavior

- Arrow buttons: press-and-hold for continuous move
- Release: sends stop
- Home button: uses ONVIF preset token (loaded from camera presets)
- AI can be enabled/disabled live from the web UI
- While AI auto-tracking is active, or while the camera is still settling after movement, manual PTZ controls are disabled in the UI

## Motion-driven PTZ + recording

When `AI_ENABLED=true`, the server starts:
- an FFmpeg raw grayscale detection pipeline (`fps=6`, padded to `640x360`)
- an EMA background-subtraction motion detector with morphology cleanup
- an ONVIF PTZ feedback loop that rate-limits movement and stops when centered
- an FFmpeg copy pipeline that keeps a 3s RAM pre-roll and writes `.ts` clips only after post-roll finishes
- camera-motion suppression that resets the detector background while PTZ/manual moves are in flight and during a short settle window after movement ends

The browser polls `/api/ai/state` and shows:
- whether AI is enabled and configured
- whether auto-tracking is active
- whether the camera is still settling after movement
- live target offset and PTZ velocity
- whether event recording is still active
