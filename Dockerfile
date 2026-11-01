# ============================================================================
# Stage 1: Compilador Rust (Linux SFU Relay + Executável Nativo Windows 11 .exe)
# ============================================================================
FROM rust:latest AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    mingw-w64 \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-pc-windows-gnu

WORKDIR /workspace

# Cachear dependências antes de compilar o código fonte
COPY Cargo.toml ./
RUN mkdir -p src/bin && \
    echo "fn main() {}" > src/main.rs && \
    echo "fn main() {}" > src/bin/sfu_relay.rs && \
    cargo build --release --bin sfu_relay && \
    cargo build --release --target x86_64-pc-windows-gnu --bin screenshare-win11 && \
    rm -rf src

# Copiar código-fonte real e compilar binários finais
COPY src ./src

# 1. Compilar o servidor Linux SFU Relay & Portal HTTP
RUN touch src/main.rs src/bin/sfu_relay.rs && \
    cargo build --release --bin sfu_relay

# 2. Compilar o executável portátil nativo do Windows 11 (screenshare-win11.exe)
RUN cargo build --release --target x86_64-pc-windows-gnu --bin screenshare-win11

# ============================================================================
# Stage 2: Imagem Runtime Leve (Servidor SFU Relay + Exportador do .exe)
# ============================================================================
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /workspace/target/release/sfu_relay /app/sfu_relay
COPY --from=builder /workspace/target/x86_64-pc-windows-gnu/release/screenshare-win11.exe /app/screenshare-win11.exe

EXPOSE 7888 7890

CMD ["/app/sfu_relay"]
