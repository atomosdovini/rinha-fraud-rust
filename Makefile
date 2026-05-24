.PHONY: docker-up docker-down docker-build push

IMAGE := ghcr.io/atomosdovini/rinha-fraud-rust:latest

docker-build:
	docker build --platform linux/amd64 --no-cache -t $(IMAGE) .

docker-up:
	docker compose up --build -d

docker-down:
	docker compose down -v

push:
	docker build --platform linux/amd64 --no-cache -t $(IMAGE) .
	docker push $(IMAGE)

verify-push:
	docker rmi $(IMAGE) 2>/dev/null || true
	docker pull $(IMAGE)
	docker run --rm --entrypoint ls $(IMAGE) -la /server /lb /index/index.bin
