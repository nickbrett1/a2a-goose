# The a2a-mcp image: one file, one dependency tree, no build step worth caching
# separately. Built on the NAS (`docker compose build`) rather than published to
# a registry, so watchtower is told to leave it alone in compose.yaml.
FROM python:3.12-slim

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY server.py .

# Unprivileged, and no shell: there is nothing here to exec.
RUN useradd --create-home --uid 10001 a2a
USER a2a

EXPOSE 8090

CMD ["python", "server.py"]
