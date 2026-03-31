FROM python:3.12-slim

RUN apt-get update && apt-get install -y --no-install-recommends fuse3 && rm -rf /var/lib/apt/lists/*

# Allow non-root FUSE mounts
RUN sed -i 's/#user_allow_other/user_allow_other/' /etc/fuse.conf || true

WORKDIR /app

# Install Python deps from PEP 723 inline metadata
RUN pip install --no-cache-dir fusepy pyyaml certifi pydantic

COPY main.py .

RUN mkdir -p /mnt/notion

ENTRYPOINT ["python", "main.py", "/mnt/notion"]
