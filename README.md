# ⭐ Janjanator

**Janjanator** é um aplicativo nativo ultra-leve para **Windows 11** (escrito 100% em Rust) de compartilhamento de tela e jogos a **60 FPS** com **áudio isolado por janela**.

### 🎯 Por que o Janjanator existe?
O projeto foi criado para **substituir o compartilhamento de tela do Discord no Brasil**, após a remoção/restrição do recurso de *Screen Share* causada por medidas e exigências absurdas do governo atual. 

Com o **Janjanator**, você e seus amigos continuam na call de voz do Discord normalmente enquanto compartilham a tela de qualquer jogo ou aplicativo em **1080p / Nativo a 60 FPS**, sem depender de servidores de terceiros, sem censura, sem limites pagos de resolução (como o Nitro) e **sem vazar a voz dos seus amigos do Discord na sua transmissão** graças ao isolamento de áudio por `PID` (`WASAPI Process Loopback`).

---

## ⚡ Principais Recursos
- **Áudio Isolado por Janela (`WASAPI Process Loopback`):** Captura apenas o som do jogo/janela escolhida. O som do Discord e do resto do Windows **não vaza** na live.
- **60 FPS Reais (`Windows.Graphics.Capture` + Direct3D 11):** Captura direto da GPU com codificação paralela multi-core e bitrate adaptativo (**Auto**, **6 Mbps**, **12 Mbps**, **25 Mbps**).
- **Zero Instalação para Amigos:** Um único arquivo portátil **`Janjanator.exe`** (~6 MB).
- **Servidor SFU Embutido + Docker Self-Hosted:** Qualquer pessoa pode hostear uma sala (até 10 pessoas) direto pelo próprio `.exe` ou via `docker-compose up -d`.

---

## 🌐 Pré-requisito de Rede (VLAN P2P)

Para que seus amigos se conectem ao seu computador sem que você precise abrir portas no roteador (e mesmo atrás de CGNAT das operadoras brasileiras), **todos os participantes da sala devem estar conectados na mesma rede virtual (VLAN)**:

- **[ZeroTier](https://www.zerotier.com/)** *(Recomendado)*
- **[Tailscale](https://tailscale.com/)**
- **[Radmin VPN](https://www.radmin-vpn.com/)**

> 💡 Basta criar uma rede gratuita no ZeroTier, Tailscale ou Radmin VPN, colocar os amigos nela e usar o **IPv4 virtual do Host** para conectar no Janjanator.

---

## 👥 Guia Rápido: Para Usuários (Amigos)

Você **não** precisa instalar Rust, Docker nem baixar o código-fonte.

1. Vá na aba **[Releases](../../releases)** deste repositório e baixe apenas o arquivo **`Janjanator.exe`**.
2. Abra o **`Janjanator.exe`** *(caso o SmartScreen do Windows avise na primeira vez, clique em **"Mais informações"** → **"Executar assim mesmo"**)*.
3. No campo **Endereço do Servidor**, digite o **IP da VLAN (ZeroTier / Radmin / Tailscale)** do amigo que está hosteando a sala.
4. Digite seu **Nickname** e clique em **🚀 Conectar à Sala**.
5. **Para assistir:** Clique em **▶ Assistir Stream** ao lado do nome do seu amigo (duplo clique no vídeo abre em **Tela Cheia**).
6. **Para transmitir sua tela/jogo:** Clique em **🖥 Compartilhar Tela / Janela**, selecione a janela do jogo ou monitor, marque **Compartilhar áudio isolado desta janela**, escolha `1080p` / `60 FPS` / `Auto` e inicie.

---

## 🖥️ Guia Rápido: Como Hostear uma Sala

Existem **duas formas** de ser o Host da sala (suporta até 10 usuários simultâneos):

### Opção 1: Host Direto pelo `Janjanator.exe` (Mais Simples — Sem Docker)
O próprio `Janjanator.exe` já possui um servidor SFU embutido na porta `7890`:
1. Abra o **`Janjanator.exe`** e, se o Firewall do Windows perguntar, marque as caixas e clique em **Permitir acesso**.
2. No campo **Endereço do Servidor**, deixe **`127.0.0.1`** e clique em **Conectar à Sala**.
3. Passe o **seu IP do ZeroTier / Radmin / Tailscale** para seus amigos colocarem no `Janjanator.exe` deles. Pronto!

### Opção 2: Host Dedicado via Docker (`docker-compose up -d`)
Se quiser deixar o servidor rodando 24/7 em background (e também compilar o `Janjanator.exe` automaticamente):
1. Instale o [Docker Desktop](https://www.docker.com/products/docker-desktop/) e clone este repositório:
   ```powershell
   git clone https://github.com/MatheusPalhares/Janjanator.git
   cd Janjanator
   ```
2. Suba o servidor com 1 comando:
   ```powershell
   docker-compose up -d --build
   ```
3. O servidor SFU ficará ativo na porta **`7890`**, um portal web de download ficará ativo em `http://SEU_IP_VLAN:7888` e o executável atualizado será exportado automaticamente em **`./dist/Janjanator.exe`**.
4. Você (no PC do Host) conecta usando **`127.0.0.1`** e seus amigos conectam usando o **seu IP da VLAN (ZeroTier / Radmin / Tailscale)**.

---

## 🛠️ Compilação Manual (Desenvolvedores)

Caso queira compilar o binário localmente no Windows 11 com Rust instalado:

```powershell
cargo build --release --bin screenshare-win11
```

Para publicar uma nova versão automática na aba **Releases** via GitHub Actions:

```powershell
git add .
git commit -m "Nova release Janjanator"
git push
git tag v1.0.0
git push origin v1.0.0
```

---

## 📄 Licença

Distribuído sob a licença **MIT**. Veja o arquivo [`LICENSE`](LICENSE) para mais detalhes. Você é livre para usar, modificar, compilar e distribuir este projeto.
