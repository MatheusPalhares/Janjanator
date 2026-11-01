# ⚡ ScreenShare Win11 Native — WGC + WASAPI PID Loopback + Docker SFU

Aplicativo desktop nativo e ultra-leve em **Rust** para **Windows 11**, projetado para compartilhamento de tela de baixa latência com **captura de áudio isolada por janela/processo (`PID`)**, suporte a **até 10 usuários simultâneos** e infraestrutura **Self-Hosted via Docker (`docker-compose up -d`)**.

---

## 🏗 Visão Geral e Arquitetura

O projeto é dividido em duas partes integradas que sobem automaticamente com um único comando:

1. **Infraestrutura Docker Self-Hosted (`docker-compose.yml`):**
   - **`screenshare-livekit-sfu` (`livekit/livekit-server:latest`):** Servidor SFU WebRTC nas portas `7880/tcp`, `7881/tcp` e `50000-50100/udp`, configurado via [`livekit.yaml`](file:///c:/Users/Teteu/Documents/New%20folder/livekit.yaml).
   - **`screenshare-sfu-hub` ([`src/bin/sfu_relay.rs`](file:///c:/Users/Teteu/Documents/New%20folder/src/bin/sfu_relay.rs)):**
     - Compila automaticamente o executável portátil do Windows 11 (`screenshare-win11.exe`) dentro do container e o exporta diretamente para a pasta local **`./dist/screenshare-win11.exe`** (sem precisar instalar compiladores no seu PC).
     - Mantém o Relay de Sala/Mídia de baixa latência na porta `7890/tcp` e um Portal Web de Download na porta `7888/tcp` (`http://localhost:7888`).

2. **Cliente Nativo Windows 11 (`screenshare-win11.exe`):**
   - **Captura de Vídeo (`Windows.Graphics.Capture` + Direct3D11):** Captura texturas direto da GPU em **30 ou 60 FPS** com fallback automático `PrintWindow/BitBlt` e redimensionamento para **720p**, **1080p** ou **Nativo (Resolução da Fonte)** ([`src/capture_video.rs`](file:///c:/Users/Teteu/Documents/New%20folder/src/capture_video.rs)).
   - **Áudio Isolado por Janela (`WASAPI Process Loopback`):** Usa a API nativa do Windows 11 `ActivateAudioInterfaceAsync` com `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS` (`PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`) para capturar **apenas o áudio do `PID` da janela escolhida**, ignorando vozes do Discord ou notificações do sistema ([`src/capture_audio.rs`](file:///c:/Users/Teteu/Documents/New%20folder/src/capture_audio.rs)).
   - **Detecção de GPU & Codificação Zero-Latency:** Detecta o encoder de hardware via Windows Media Foundation (`MFTEnumEx` — NVENC / AMF / QuickSync) e transmite pacotes comprimidos em bitrates selecionáveis (**6 Mbps**, **12 Mbps**, **25 Mbps**) com geração de Keyframes sob demanda ([`src/codec.rs`](file:///c:/Users/Teteu/Documents/New%20folder/src/codec.rs)).
   - **Autenticação JWT Local:** Gera tokens JWT `HS256` compatíveis com o LiveKit localmente no app ([`src/network.rs`](file:///c:/Users/Teteu/Documents/New%20folder/src/network.rs)).

---

## 🚀 Guia do Host (Como Subir o Servidor no seu PC)

### 1. Subir toda a ferramenta com 1 comando
Na raiz do projeto, execute no terminal (PowerShell):

```powershell
docker-compose up -d
```

O Docker irá:
1. Baixar e iniciar o servidor **LiveKit SFU** (`screenshare-livekit-sfu`).
2. Compilar o executável portátil **`screenshare-win11.exe`** em modo `--release` e disponibilizá-lo automaticamente na pasta:
   - 📁 `.\dist\screenshare-win11.exe`
   - 🌐 E também para download direto no navegador em: `http://localhost:7888`

Para verificar se os containers estão rodando:

```powershell
docker-compose ps
```

---

### 2. Como Conectar seus Amigos ao seu Servidor (Opções de Rede)

#### 🟣 Opção 1: Playit.gg (Mais Prática — Sem instalar VPN nos amigos e sem abrir portas)
Como toda a comunicação de sala, vídeo e áudio isolado do aplicativo passa pela porta TCP **`7890`**, você pode criar um único túnel no **Playit.gg**:
1. Abra o agente do **Playit.gg** no seu PC e acesse o painel web do Playit.
2. Crie um novo túnel com as seguintes configurações:
   - **Tunnel Type:** `TCP` (Custom TCP)
   - **Local Address:** `127.0.0.1`
   - **Local Port:** `7890`
3. O Playit irá gerar um endereço público no formato `exemplo.gl.at.ply.gg:45123` (ou `147.185.x.y:45123`).
4. Basta enviar esse endereço `HOST:PORTA` para os seus amigos colarem diretamente no campo **"Endereço do Servidor"** do `screenshare-win11.exe`!

#### 🟢 Opção 2: ZeroTier ou Tailscale (Zero Configuração de Roteador via Rede Virtual)
Ideal para menor latência possível (conexão direta P2P UDP Hole Punching entre os PCs):
1. Conecte você e seus amigos na mesma rede do **ZeroTier** (ou Tailscale).
2. Copie o seu **IPv4 Managed do ZeroTier** (ex: `10.147.x.y` ou `192.168.192.x`).
3. Passe apenas esse IP (sem porta) para os seus amigos colocarem no `screenshare-win11.exe` (o app usará a porta `7890` automaticamente).

#### 🔵 Opção 3: Port Forwarding no Roteador (Conexão Direta via IP Público)
Se preferir que seus amigos conectem diretamente pelo seu IP público sem instalar VPN:
1. Acesse o painel do seu roteador e crie regras de **Port Forwarding (Redirecionamento de Portas)** apontando para o IP LAN do seu PC:
   - **TCP `7880`** (Sinalização LiveKit)
   - **TCP `7881`** (Fallback WebRTC TCP)
   - **TCP `7890`** (Relay SFU Nativo)
   - **TCP `7888`** *(Opcional — Portal HTTP para amigos baixarem o `.exe` direto do seu PC)*
   - **UDP `50000` até `50100`** (Fluxo de mídia WebRTC de baixa latência)
2. Libere essas mesmas portas no Firewall do Windows (PowerShell como Administrador):
   ```powershell
   New-NetFirewallRule -DisplayName "ScreenShare Win11 TCP" -Direction Inbound -Protocol TCP -LocalPort 7880,7881,7888,7890 -Action Allow
   New-NetFirewallRule -DisplayName "ScreenShare Win11 UDP" -Direction Inbound -Protocol UDP -LocalPort 50000-50100 -Action Allow
   ```

---

## 👥 Guia dos Usuários (Para os seus Amigos)

### 1. Como Baixar o Executável (`screenshare-win11.exe`)
Seus amigos têm duas formas simples de baixar o aplicativo portátil (não precisa instalar nada):
- **Via GitHub Releases (Repositório Privado):**
  1. Acesse o repositório privado no GitHub para o qual você deu permissão de leitura a eles.
  2. Clique na aba **Releases** (na barra lateral direita do GitHub).
  3. Na versão mais recente, clique em **`screenshare-win11.exe`** em *Assets* para baixar.
- **Via Portal Direto do Host Docker:**
  1. Com o servidor rodando, basta abrir `http://<IP_DO_HOST>:7888` no navegador e clicar em **"Baixar screenshare-win11.exe"**.

### 2. Como Entrar na Sala e Assistir
1. Dê duplo clique no **`screenshare-win11.exe`**.
2. No campo **IP / Host do Servidor**, digite o IP do Host (ou `127.0.0.1` se você for o próprio Host no mesmo PC).
3. Digite seu **Nickname** e clique em **🚀 Conectar à Sala**.
4. Na barra lateral esquerda (**Usuários na Sala**), quando alguém estiver transmitindo aparecerá o selo **● AO VIVO** com a resolução, FPS e status de áudio.
5. Clique em **▶ Assistir Transmissão**:
   - **Tela Cheia:** Dê **duplo clique** sobre o vídeo (ou clique no botão `⛶ Tela Cheia` / tecla `ESC` para sair).
   - **Controle de Volume:** Ajuste o slider **🔊 Volume (0% a 200%)** na barra superior do player.
   - **Parar de Assistir / Economizar Banda:** Clique em **⏹ Parar de Assistir** ou deixe marcada a opção *"Pausar stream ao minimizar"*.

### 3. Como Compartilhar sua Tela com Áudio Isolado por Janela
1. Conectado à sala, clique no botão azul **🖥 Compartilhar Tela / Janela** no topo.
2. Escolha entre as abas:
   - **🪟 Janelas Abertas (Áudio Isolado por PID):** Lista todas as janelas abertas no seu Windows 11 com seus respectivos `PID`s.
   - **🖥 Monitores Inteiros:** Lista todos os seus monitores físicos.
3. Marque a caixa **🔊 Compartilhar áudio desta janela (WASAPI Process Loopback Isolado)**:
   - O aplicativo capturará **apenas o som daquele jogo/aplicativo**, sem vazar o áudio dos seus amigos falando no Discord nem sons do sistema.
4. Escolha a qualidade desejada:
   - **Resolução:** `720p (1280x720)` | `1080p (1920x1080)` | `Nativo (Resolução da Fonte)`
   - **Taxa de Quadros:** `30 FPS` | `60 FPS (Fluido)`
   - **Bitrate Alvo:** `Médio (6 Mbps)` | `Alto (12 Mbps)` | `Ultra (25 Mbps)`
5. Clique em **🔴 Iniciar Transmissão Agora**.
   - Se a janela compartilhada for minimizada ou redimensionada, o app ajusta automaticamente sem travar. Se ela for fechada, a transmissão é encerrada de forma limpa.

---

## 📦 Compilação Manual e Publicação no GitHub Releases (Repositório Privado)

### Compilar Manualmente no Windows (Opcional)
Caso tenha o Rust instalado localmente no Windows e queira compilar fora do Docker:

```powershell
cargo build --release --bin screenshare-win11
```
O binário final otimizado (`lto = true`, `strip = true`) será gerado em `target\release\screenshare-win11.exe`.

### Publicar o Executável no seu Repositório Privado do GitHub (Releases Automáticas)
O projeto já inclui o workflow completo do GitHub Actions em [`.github/workflows/release.yml`](file:///c:/Users/Teteu/Documents/New%20folder/.github/workflows/release.yml). Sempre que você enviar uma tag `v*` (ou disparar manualmente na aba *Actions*), o GitHub compila o `.exe` no `windows-latest` e cria a Release anexando o `screenshare-win11.exe`.

Passo a passo para subir para o seu repositório privado:

```powershell
# 1. Inicializar o repositório Git e adicionar os arquivos
git init
git add .
git commit -m "feat: app nativo screenshare win11 + docker sfu"

# 2. Conectar ao seu repositório privado no GitHub
git branch -M main
git remote add origin https://github.com/SEU_USUARIO/SEU_REPO_PRIVADO.git
git push -u origin main

# 3. Criar e enviar a tag v0.1.0 para gerar a Release com o .exe automaticamente!
git tag v0.1.0
git push origin v0.1.0
```

