podman run --replace \
	--name go2rtc \
	--env-file .env \
	-p 1984:1984 \
	-p 8555:8555 \
	-v "$(pwd)/go2rtc.yaml:/config/go2rtc.yaml:ro,Z" \
	ghcr.io/alexxit/go2rtc:latest
