# 🤖 Sword Bot Online RS — Guida Completa di Installazione e Avvio

Benvenuto nella guida ufficiale per configurare, compilare ed eseguire **Sword Bot Online RS**, il bot per Discord sviluppato in Rust con pannello di controllo web integrato.

---

## 1. Struttura del progetto

Il progetto è composto da due componenti principali:

- **`sword_bot_online_rs/`** — Backend Rust: bot Discord (Poise/Serenity), server web **Axum**, API REST, overlay per OBS (browser source) e database **SQLite**.
- **`sfo-site/`** — Frontend web: pannello di controllo e dashboard statistiche in **Vite + React**, servito dal backend Rust sotto `/staff/`.

---

## 📋 Indice
1. [Struttura del progetto](#1-struttura-del-progetto)
2. [Configurazione delle Variabili d'Ambiente (`.env`)](#2-configurazione-delle-variabili-dambiente-env)
3. [Modalità 1: Compilazione ed esecuzione sul proprio Sistema Operativo (Locale / Sviluppo)](#3-modalità-1-compilazione-ed-esecuzione-sul-proprio-sistema-operativo-locale--sviluppo)
4. [Modalità 2: Compilazione e caricamento su una Macchina Virtuale (VM Linux Ubuntu 24.04)](#4-modalità-2-compilazione-e-caricamento-su-una-macchina-virtuale-vm-linux-ubuntu-2404)
5. [Modalità 3: Hosting su piattaforme gratuite per bot Discord (PaaS / Bot-Hosting / Docker)](#5-modalità-3-hosting-su-piattaforme-gratuite-per-bot-discord-paas--bot-hosting--docker)
6. [Comandi utili e gestione dei backup](#6-comandi-utili-e-gestione-dei-backup)

---

## 2. Configurazione delle Variabili d'Ambiente (`.env`)

Il progetto include un file modello denominato `env.example.txt` (facilmente apribile e modificabile con qualsiasi editor di testo anche su macOS). 

Per configurare il bot:
1. Fai una copia del file `env.example.txt` e rinominala in `.env` (il punto iniziale è importante affinché il bot lo riconosca):
   ```bash
   cp env.example.txt .env
   ```
2. Apri il file `.env` appena creato e inserisci i tuoi dati personali (token Discord, ID del server, credenziali OAuth2, ecc.). Troverai spiegazioni dettagliate riga per riga all'interno del file.

---

## 3. Modalità 1: Compilazione ed esecuzione sul proprio Sistema Operativo (Locale / Sviluppo)

Ideale se vuoi eseguire e testare il bot direttamente sul tuo PC (Windows, macOS o Linux).

### Requisiti:
- **Rust** (versione stabile recente, consigliato Rust 1.85+ con Cargo). Installabile da [rustup.rs](https://rustup.rs/).
- **SQLite3** installato nel sistema.
- **Node.js 20+** e **npm** per la build del frontend (pannello staff). Installabili da [nodejs.org](https://nodejs.org/).

### Passaggi:
1. Apri un terminale nella cartella principale del progetto.
2. Assicurati di aver creato e configurato il file `.env` partendo da `env.example.txt`.
3. **Compila il frontend** (pannello staff / dashboard statistiche) — il backend Rust serve i file statici da `sfo-site/dist/`:
   ```bash
   cd sfo-site && npm install && npm run build
   cd ..
   ```
4. Compila ed esegui il bot in modalità di rilascio (`release`), che garantisce le massime prestazioni:
   ```bash
   cargo run --release --manifest-path sword_bot_online_rs/Cargo.toml
   ```
   *(Oppure entra nella cartella `sword_bot_online_rs` con `cd sword_bot_online_rs` e digita `cargo run --release`)*.

---

## 4. Modalità 2: Compilazione e caricamento su una Macchina Virtuale (VM Linux Ubuntu 24.04)

Se possiedi una VPS o una VM Linux (Ubuntu 24.04 LTS) e desideri far girare il bot stabilmente 24/7 con HTTPS automatico via Caddy:

1. **Connettiti via SSH** alla VM e installa gli strumenti di compilazione + Node.js 20+ per il frontend:
   ```bash
   sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev sqlite3 nodejs npm
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source "$HOME/.cargo/env"
   ```
2. **Clona o trasferisci** i file del progetto sulla VM (es. in `/opt/sword-bot/`).
3. **Compila il frontend** (pannello staff) — genera i file statici in `sfo-site/dist/`:
   ```bash
   cd /opt/sword-bot/sfo-site && npm install && npm run build
   cd /opt/sword-bot
   ```
4. **Compila il backend in release** — genera il binario `sword_bot_online_rs` in `target/release/`:
   ```bash
   cargo build --release --manifest-path sword_bot_online_rs/Cargo.toml
   ```
5. **Configura il file `.env`** nella cartella principale (`/opt/sword-bot/.env`) con **tutti i valori reali** (niente placeholder). Riferimento esatto da `env.example.txt`:
   ```env
   DISCORD_TOKEN=MTU2ODMyMDE5OTk4MjA1NTUzNA.GELqSn...
   GUILD_ID=976563839564935188
   SFO_SITE_DIR=/opt/sword-bot/sfo-site/dist
   EVENT_OVERLAY_HOST=0.0.0.0
   EVENT_OVERLAY_PORT=8080
   DB_PATH=/opt/sword-bot/sword_bot.db
   BACKUP_DIR=/opt/sword-bot/backups
   EVENT_OVERLAY_PUBLIC_URL=https://tuodominio.sslip.io
   DISCORD_OAUTH_CLIENT_ID=123456789012345678
   DISCORD_OAUTH_CLIENT_SECRET=il_tuo_client_secret
   DISCORD_OAUTH_REDIRECT_URI=https://tuodominio.sslip.io/staff/auth/callback
   SUPABASE_URL=https://tuoprogetto.supabase.co
   # Opzionale:
   # BACKUP_ENCRYPTION_KEY=la_tua_chiave_segreta
   # BOT_AUDIT_CHANNEL_ID=123456789012345678
   # SUPABASE_SERVICE_ROLE_KEY=...
   ```
   > **Importante**: `EVENT_OVERLAY_PUBLIC_URL` e `DISCORD_OAUTH_REDIRECT_URI` devono usare il dominio `.sslip.io` (es. `https://192-0-2-1.sslip.io`) che Caddy gestirà con HTTPS automatico. Sostituisci `tuodominio.sslip.io` con il tuo IP pubblico nel formato `xxx-xxx-xxx-xxx.sslip.io`.
6. **Configura Caddy** come reverse proxy con HTTPS automatico (crea `/etc/caddy/Caddyfile`):
   ```caddyfile
   tuodominio.sslip.io {
       reverse_proxy localhost:8080
   }
   ```
   Poi riavvia Caddy:
   ```bash
   sudo systemctl reload caddy
   ```
   > Caddy otterrà e rinnoverà automaticamente i certificati TLS per `*.sslip.io`.
7. **Crea il servizio systemd** (`/etc/systemd/system/swordbot.service`) puntando al **binario compilato**, non a `cargo run`:
   ```ini
   [Unit]
   Description=Sword Bot Online RS
   After=network.target

   [Service]
   Type=simple
   User=swordbot
   WorkingDirectory=/opt/sword-bot
   ExecStart=/opt/sword-bot/sword_bot_online_rs/target/release/sword_bot_online_rs
   Restart=always
   EnvironmentFile=/opt/sword-bot/.env

   [Install]
   WantedBy=multi-user.target
   ```
   > Crea l'utente dedicato: `sudo useradd -r -s /bin/false swordbot && sudo chown -R swordbot:swordbot /opt/sword-bot`
8. **Abilita e avvia** il servizio:
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable swordbot
   sudo systemctl start swordbot
   sudo systemctl status swordbot
   ```

---

## 5. Modalità 3: Hosting su piattaforme gratuite per bot Discord (PaaS / Bot-Hosting / Docker)

Molti pannelli di hosting per bot Discord (es. pannelli Pterodactyl, Bot-Hosting.net, Railway, Fly.io) richiedono di caricare i file sorgente o un archivio compresso (`deploy_bundle.tar.gz`).

### Linee guida per l'hosting cloud:
1. **Compilazione ed Esecuzione**: L'hosting deve disporre di un ambiente Rust stable recente (il progetto utilizza Rust edition 2024). Il comando di avvio configurato nel pannello deve essere:
   ```bash
   cargo run --release
   ```
   *(Se il pannello richiede una fase di build separata, inserisci: `cargo build --release`)*.
2. **Variabili d'Ambiente**: Non inserire mai token o credenziali nei file caricati nello ZIP. Usa `env.example.txt` come riferimento delle variabili necessarie, inserendo poi tutte le chiavi (es. `DISCORD_TOKEN`, `DISCORD_OAUTH_CLIENT_SECRET`, `GUILD_ID`, `EVENT_OVERLAY_PORT`, `EVENT_OVERLAY_HOST`, `DB_PATH`, `BACKUP_DIR`, `EVENT_OVERLAY_PUBLIC_URL`, `DISCORD_OAUTH_CLIENT_ID`, `DISCORD_OAUTH_REDIRECT_URI`, `SUPABASE_URL`) direttamente nella sezione **Variabili d'Ambiente / Segreti** del pannello di hosting.
3. **Persistenza dei Dati (Database)**: Assicurati che il database SQLite non venga cancellato ai riavvi. Configura il percorso nel pannello puntando alla cartella persistente del container:
   ```env
   DB_PATH=/home/container/sword_bot.db
   BACKUP_DIR=/home/container/backups
   ```
4. **Porta Web**: Se il pannello assegna una porta specifica per il traffico web, assicurati di mappare le variabili:
   ```env
   EVENT_OVERLAY_PORT=<porta_assegnata_dal_pannello>
   EVENT_OVERLAY_HOST=0.0.0.0
   ```
5. **URL Pubblico e OAuth2**: Per il pannello staff e l'overlay OBS, imposta l'URL pubblico fornito dalla piattaforma (es. `https://tuo-bot.fly.dev` o `https://tuo-bot.railway.app`):
   ```env
   EVENT_OVERLAY_PUBLIC_URL=https://tuo-bot.fly.dev
   DISCORD_OAUTH_REDIRECT_URI=https://tuo-bot.fly.dev/staff/auth/callback
   ```
   > **Nota**: Su PaaS il reverse proxy / TLS è gestito dalla piattaforma (Fly.io, Railway, ecc.). **Non serve Caddy** — la piattaforma termina HTTPS e inoltra traffico HTTP alla porta del container.
6. **Frontend (pannello staff)**: Se il pannello permette build steps separati, esegui prima la build del frontend:
   ```bash
   cd sfo-site && npm install && npm run build
   ```
   I file statici finiscono in `sfo-site/dist/` e vengono serviti automaticamente dal backend Rust sotto `/staff/`.

---

## 6. Comandi utili e gestione dei backup

- **Ripristino di un backup del database**:
  Il bot include uno strumento da riga di comando per ripristinare i backup in sicurezza:
  ```bash
  cargo run --bin restore_backup --manifest-path sword_bot_online_rs/Cargo.toml
  ```
- **Controllo dello stato (Health Check)**:
  Il server web integrato espone un endpoint `/health` che restituisce lo stato operativo del bot e dei servizi connessi.
