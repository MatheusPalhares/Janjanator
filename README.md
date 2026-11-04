# ⭐ Janjanator

**Janjanator** é um aplicativo nativo ultra-leve para **Windows 11** (escrito 100% em Rust) de compartilhamento de tela e jogos a **60 FPS** com **áudio isolado por janela**.

### 🎯 Por que o Janjanator existe?
O projeto foi criado para **substituir o compartilhamento de tela do Discord no Brasil**, após a remoção/restrição do recurso de *Screen Share* causada por medidas e exigências absurdas do governo atual. 

Com o **Janjanator**, você e seus amigos continuam na call de voz do Discord normalmente enquanto compartilham a tela de qualquer jogo ou aplicativo em **1080p / Nativo a 60 FPS**, sem depender de servidores de terceiros, sem censura, sem limites pagos de resolução (como o Nitro) e **sem vazar a voz dos seus amigos do Discord na sua transmissão** graças ao isolamento de áudio por `PID` (`WASAPI Process Loopback`).

---

## ⚡ Principais Recursos
- **Áudio Isolado por Janela (`WASAPI Process Loopback`):** Captura apenas o som do jogo/janela escolhida. O som do Discord e do resto do Windows **não vaza** na live.
- **Tela Inteira sem Eco do Discord:** Ao compartilhar o monitor inteiro, o áudio do Windows é transmitido **sem as vozes da call e os sons do Discord** (Stable, PTB e Canary detectados automaticamente). Pode ser desligado na tela de seleção da transmissão.
- **60 FPS Reais (`Windows.Graphics.Capture` + Direct3D 11):** Captura direto da GPU com codificação paralela multi-core e bitrate adaptativo (**Auto**, **6 Mbps**, **12 Mbps**, **25 Mbps**).
- **Suporte a Túnel Cloudflare (TLS/WSS Nativo):** Conecte através de túneis públicos seguros (`live.seudominio.com.br`) sem precisar que seus amigos instalem VPN/VLAN. Suporte nativo a certificados raiz Mozilla (`webpki-roots`) e criptografia `Ring`.
- **Zero Instalação para Amigos:** Um único arquivo portátil **`Janjanator.exe`** (~7 MB).
- **Servidor SFU Embutido + Docker Self-Hosted:** Qualquer pessoa pode hostear uma sala (até 10 pessoas) direto pelo próprio `.exe` ou via `docker-compose up -d` com túnel integrado.

---

## 🌐 Conectividade: Como Conectar os Amigos

Para que seus amigos se conectem ao seu computador mesmo atrás de CGNAT de operadoras de internet, você pode escolher **duas opções de rede**:

### ☁️ Opção A: Cloudflare Tunnel *(Recomendado — Sem VPN para os amigos)*
Se você possui um domínio próprio gerenciado na Cloudflare (gratuito), você pode subir o serviço `cloudflared` embutido no `docker-compose`. 
- **Vantagem:** Seus amigos **NÃO precisam instalar ZeroTier, Tailscale nem nenhuma VPN**.
- Eles apenas digitam o seu subdomínio (ex: `live.meudominio.com.br`) diretamente no Janjanator. O app gerencia a criptografia TLS/WSS na porta `443` automaticamente.

### 🔗 Opção B: VLAN P2P *(ZeroTier / Tailscale / Radmin VPN)*
Caso você não tenha um domínio próprio ou não use Cloudflare, todos os participantes entram na mesma rede virtual:
- **[ZeroTier](https://www.zerotier.com/)** *(Recomendado)*
- **[Tailscale](https://tailscale.com/)**
- **[Radmin VPN](https://www.radmin-vpn.com/)**

> 💡 Basta criar uma rede gratuita no ZeroTier/Tailscale/Radmin, colocar os amigos nela e usar o **IPv4 virtual do Host** no campo de servidor.

---

## 👥 Guia Rápido: Para Usuários (Amigos)

Você **não** precisa instalar Rust, Docker nem baixar o código-fonte.

1. Vá na aba **[Releases](../../releases)** deste repositório e baixe apenas o arquivo **`Janjanator.exe`**.
2. Abra o **`Janjanator.exe`** *(caso o SmartScreen do Windows avise na primeira vez, clique em **"Mais informações"** → **"Executar assim mesmo"**)*.
3. No campo **Endereço do Servidor**, digite o **domínio do túnel Cloudflare** (ex: `live.meuamigo.com.br`) OU o **IP da VLAN (ZeroTier / Radmin / Tailscale)** fornecido por quem está hosteando a sala.
4. Digite seu **Nickname** e clique em **🚀 Conectar à Sala**.
5. **Para assistir:** Clique em **▶ Assistir Stream** ao lado do nome do seu amigo (duplo clique no vídeo abre em **Tela Cheia**).
6. **Para transmitir sua tela/jogo:** Clique em **🖥 Compartilhar Tela / Janela**, selecione a janela do jogo ou monitor, marque **Compartilhar áudio isolado desta janela**, escolha `1080p` / `60 FPS` / `Auto` e inicie.
7. **Para atualizar quando sair versão nova:** Basta clicar no botão **🔄 Atualizar App (GitHub Release)** na tela inicial ou no topo do aplicativo — ele baixa a versão `latest` do GitHub, substitui o `.exe` e reinicia sozinho.

---

## 🖥️ Guia Rápido: Como Hostear uma Sala

Existem **três formas** de ser o Host da sala (suporta até 10 usuários simultâneos):

### Opção 1: Host Direto pelo `Janjanator.exe` (Mais Simples — Sem Docker / Com VLAN)
O próprio `Janjanator.exe` já possui um servidor SFU embutido na porta `7890`:
1. Abra o **`Janjanator.exe`** e, se o Firewall do Windows perguntar, marque as caixas e clique em **Permitir acesso**.
2. No campo **Endereço do Servidor**, deixe **`127.0.0.1`** e clique em **Conectar à Sala**.
3. Passe o **seu IP do ZeroTier / Radmin / Tailscale** para seus amigos colocarem no `Janjanator.exe` deles. Pronto!

---

### Opção 2: Host Dedicado via Docker + Cloudflare Tunnel (⭐ Recomendado — Sem VPN para os amigos!)
Se você possui um domínio na Cloudflare, pode rodar o servidor 24/7 com o túnel embutido. Seus amigos só precisam do domínio e **não precisam de nenhuma VPN instalada**:

1. **Criar o Túnel no Cloudflare Zero Trust:**
   - Acesse o painel da [Cloudflare Zero Trust](https://one.dash.cloudflare.com/) → **Networks** → **Tunnels** → **Add a tunnel**.
   - Escolha o conector **Cloudflared** e dê um nome (ex: `janjanator-hub`).
   - Na tela de instalação, copie apenas o **Token** do túnel (o código que começa após `--token`).
   - Na aba **Public Hostname** (Rotas / Published Application), adicione a rota para o relay SFU:
     - **Subdomain:** `live` *(ou qualquer outro)*
     - **Domain:** selecione o seu domínio (ex: `meudominio.com.br`)
     - **Service:** selecione `HTTP` e no endereço digite `screenshare-hub:7890` *(pois o container cloudflared comunica direto na rede interna do Docker)*.
     - Clique em **Save tunnel**.

2. **Configurar o `.env` no Docker:**
   - Na pasta do projeto, crie o arquivo `.env` (você pode copiar do `.env.example`):
     ```powershell
     cp .env.example .env
     ```
   - Abra o `.env` e defina seu token da Cloudflare:
     ```env
     CLOUDFLARE_TUNNEL_TOKEN=seu_token_aqui_da_cloudflare
     ```

3. **Subir os containers:**
   ```powershell
   docker-compose up -d --build
   ```
   - O servidor SFU (`screenshare-hub`), o LiveKit e o túnel `cloudflared` subirão juntos automaticamente.
   - O executável atualizado com suporte nativo a WSS/TLS será exportado em **`./dist/Janjanator.exe`**.

4. **Conectar:**
   - **Você (Host):** Conecta pelo `Janjanator.exe` usando `127.0.0.1` ou o próprio domínio.
   - **Seus Amigos:** Abrem o `Janjanator.exe`, digitam apenas o domínio (ex: `live.meudominio.com.br`), colocam o Nickname e clicam em Conectar!

---

### Opção 3: Host Dedicado via Docker (Sem Cloudflare — Apenas VLAN)
Se quiser rodar via Docker mas sem domínio público da Cloudflare:
1. Deixe a variável `CLOUDFLARE_TUNNEL_TOKEN` vazia no `.env`.
2. Suba com `docker-compose up -d`.
3. O servidor SFU ficará ativo na porta **`7890`** e um portal web para download ficará em `http://SEU_IP_VLAN:7888`.
4. Você conecta com `127.0.0.1` e seus amigos conectam pelo **seu IP do ZeroTier / Tailscale / Radmin**.

---

## 🛡️ Avisos do Windows (SmartScreen e Firewall) — Por que aparecem?

Ao abrir o **`Janjanator.exe`** pela primeira vez, o Windows exibirá dois avisos padrões. **Isso é totalmente normal em projetos Open Source independentes:**

1. **Tela Azul do Windows SmartScreen (*"O Windows protegeu o computador"*):**
   - **Por que acontece:** A Microsoft exige um certificado digital comercial pago (que custa centenas de dólares por ano) para que um `.exe` baixado da internet seja reconhecido automaticamente. Como o **Janjanator** é um projeto gratuito, independente e de código 100% aberto, ele ainda não possui assinatura digital comercial.
   - **Como abrir:** Clique em **"Mais informações"** e depois no botão **"Executar assim mesmo"**.
2. **Alerta do Firewall do Windows (*"O Windows Defender Firewall bloqueou alguns recursos deste aplicativo"*):**
   - **Por que acontece:** O `Janjanator.exe` traz um mini-servidor SFU embutido que abre a **porta TCP `7890`** localmente para permitir que você ou qualquer amigo possa hostear uma sala sem precisar instalar nada extra. Por abrir a porta `7890` para conexão P2P na VLAN, o Windows pede sua confirmação.
   - **Como autorizar:** Marque as duas caixinhas (**Redes privadas** e **Redes públicas**) e clique em **"Permitir acesso"**.

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
