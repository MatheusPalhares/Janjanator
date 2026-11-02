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

# Copiar recursos de ícone/metadados e código-fonte real
COPY img ./img
COPY src ./src

# 1. Compilar o servidor Linux SFU Relay & Portal HTTP
RUN rm -f target/release/sfu_relay target/release/deps/sfu_relay* && \
    touch src/*.rs src/bin/*.rs && \
    cargo build --release --bin sfu_relay

# 2. Compilar recurso Win32 (.ico + VERSIONINFO "Janjanator") e linkar no executável final (Janjanator.exe)
RUN x86_64-w64-mingw32-windres img/janjanator.rc -O coff -o img/janjanator_res.o && \
    rm -f target/x86_64-pc-windows-gnu/release/screenshare-win11.exe target/x86_64-pc-windows-gnu/release/deps/screenshare_win11* && \
    touch src/*.rs && \
    cargo rustc --release --target x86_64-pc-windows-gnu --bin screenshare-win11 -- -C link-arg=/workspace/img/janjanator_res.o

# ============================================================================
# Stage 2: Imagem Runtime Leve (Servidor SFU Relay + Exportador do Janjanator.exe)
# ============================================================================
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /workspace/target/release/sfu_relay /app/sfu_relay
COPY --from=builder /workspace/target/x86_64-pc-windows-gnu/release/screenshare-win11.exe /app/Janjanator.exe

EXPOSE 7888 7890

CMD ["/app/sfu_relay"]
