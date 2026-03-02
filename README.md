# cam_rs

Rust API + web UI for:
- live camera video via WebRTC (through go2rtc)
- ONVIF PTZ control (move, stop, presets)

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

Note: `RTSP_URL` is used by `go2rtc.yaml`, not directly by the Rust API.

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
- `POST /api/onvif/move`
- `POST /api/onvif/stop`
- `POST /api/onvif/goto-preset`

## PTZ behavior

- Arrow buttons: press-and-hold for continuous move
- Release: sends stop
- Home button: uses ONVIF preset token (loaded from camera presets)
