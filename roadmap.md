# Roadmap: App Nativo de Screen Share Leve (Windows 11)

## Visão Geral da Stack Recomendada
- **Linguagem Principal:** Rust
- **APIs de Captura (Windows 11):** `Windows.Graphics.Capture` (Vídeo) + `WASAPI Process Loopback` (Áudio por PID)
- **Codificação:** Hardware H.264/HEVC (Media Foundation / NVENC) + Opus (Áudio)
- **Transporte e Rede:** WebRTC via LiveKit (Servidor Único em Docker + SDK Rust)
- **Interface Gráfica:** Tauri v2 ( pela facilidade de renderizar vídeo WebRTC) ou `egui` + `wgpu` (100% nativo)

---

## Fase 1: Protótipo de Captura Local (Vídeo e Áudio)
O objetivo desta fase é capturar uma janela específica e salvar em um arquivo local ou exibir na tela, sem rede envolvida.

- [x] **1.1. Listagem de Janelas e Monitores:**
  - Usar a API do Windows (`EnumWindows` / `EnumDisplayMonitors`) para listar títulos de janelas abertas e obter seus `HWND` (Window Handles) e `PID` (Process IDs).
- [x] **1.2. Captura de Vídeo com `Windows.Graphics.Capture`:**
  - Implementar captura de frames (`Direct3D11CaptureFramePool`) a partir de um `HWND` ou `HMONITOR`.
  - Configurar captura a 60 FPS em textura D3D11 (usando crates como `windows-capture` ou `scap` para acelerar o desenvolvimento).
- [x] **1.3. Redimensionamento na GPU (Seletor de Qualidade):**
  - Implementar downscaling via Direct3D11 VideoProcessor ou shader simples para converter a textura capturada para **720p**, **1080p** ou manter **Nativo**.
- [x] **1.4. Captura de Áudio Isolada (WASAPI Process Loopback):**
  - Usar `ActivateAudioInterfaceAsync` passando `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS` no modo `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE` com o `PID` da janela escolhida.
  - Capturar o buffer PCM bruto em uma thread dedicada.

---

## Fase 2: Codificação de Baixa Latência (Hardware Encoding)
Transformar os frames brutos (que pesam gigabytes por minuto) em pacotes leves prontos para envio em tempo real.

- [x] **2.1. Pipeline de Áudio (Opus):**
  - Integrar o codec `opus` para comprimir o buffer PCM do WASAPI em frames de 10ms ou 20ms (48kHz Estéreo, ~128kbps).
- [x] **2.2. Pipeline de Vídeo (H.264 / AV1 via GPU):**
  - Integrar Media Foundation (`IMFTransform`) ou `ffmpeg-next` para usar os encoders de hardware (`h264_nvenc`, `h264_amf`, `h264_qsv`).
  - Configurar o encoder para **Zero-Latency** (sem B-frames, preset `llhp` / `ull`, CBR/VBR controlado por bitrate: ex. 4 Mbps para 720p, 8-12 Mbps para 1080p, 15-25 Mbps para Nativo).
  - Implementar geração de Keyframes (IDR) sob demanda para quando um novo usuário entrar na sala.

---

## Fase 3: Infraestrutura do Servidor (Docker no seu PC)
Subir o servidor que vai gerenciar a sala e distribuir a stream para os até 10 usuários.

- [x] **3.1. Setup do Servidor SFU (LiveKit Server):**
  - Criar um `docker-compose.yml` rodando a imagem `livekit/livekit-server` em modo standalone.
  - Configurar o arquivo `livekit.yaml` definindo uma chave de API fixa (já que é um app privado para amigos) e a faixa de portas UDP (ex: `50000-50100`).
- [x] **3.2. Conectividade Externa:**
  - *Opção A (Port Forwarding):* Liberar a porta TCP `7880` (Sinalização) e UDP `50000-50100` (Mídia) no roteador apontando para o seu PC.
  - *Opção B (Zero-Config):* Instalar Tailscale/ZeroTier nos PCs dos amigos ou embutir um túnel leve.

---

## Fase 4: Integração de Rede no Cliente (Transmissão e Recepção)
Conectar o app Rust ao servidor Docker.

- [x] **4.1. Conexão à Sala:**
  - Integrar o SDK (`livekit` crate em Rust ou WebRTC nativo).
  - Gerar um token JWT localmente no app (usando a chave secreta embutida ou um mini-endpoint no Docker) para entrar na sala única com o nickname do usuário.
- [x] **4.2. Publicação das Tracks (Host):**
  - Pegar os pacotes H.264 do encoder e os pacotes Opus do WASAPI e injetá-los nas tracks de vídeo (`LocalVideoTrack`) e áudio (`LocalAudioTrack`) da sessão WebRTC.
- [x] **4.3. Consumo das Tracks (Viewer):**
  - Receber a track de vídeo e áudio de quem está transmitindo, decodificar via hardware e enviar para o player da janela e saída de som padrão do Windows.

---

## Fase 5: Interface do Usuário (UX Minimalista)
Construir uma interface limpa focada apenas no propósito do app.

- [x] **5.1. Tela de Conexão:**
  - Campo simples para `IP do Servidor`, `Nickname` e botão `Conectar`.
- [x] **5.2. Barra de Controles e Seleção de Stream:**
  - Lista de usuários conectados na sala (até 10).
  - Modal de "Compartilhar Tela":
    - Aba de Monitores / Aba de Janelas Abertas.
    - Checkbox: *"Compartilhar áudio desta janela"*.
    - Dropdown de Resolução: `720p` | `1080p` | `Nativo (Fonte)` e Taxa de Quadros (`30 FPS` | `60 FPS`).
    - Dropdown de Bitrate: `Médio (6 Mbps)` | `Alto (12 Mbps)` | `Ultra (25 Mbps)`.
- [x] **5.3. Player de Visualização:**
  - Tela cheia com clique duplo, controle de volume local do stream que está sendo assistido e botão de "Parar de Assistir" (para economizar banda quando minimizado).

---

## Fase 6: Polimento e Distribuição para os Amigos
- [x] **6.1. Tratamento de Eventos de Janela:**
  - Lidar com situações onde a janela capturada é minimizada, redimensionada ou fechada (parar a stream graciosamente sem dar crash).
- [x] **6.2. Empacotamento (Single Executable):**
  - Compilar em modo `--release` com otimizações LTO (`lto = true` no `Cargo.toml`) gerando um `.exe` único e portátil (normalmente entre 5 MB e 15 MB em Rust) para enviar diretamente aos seus amigos.
