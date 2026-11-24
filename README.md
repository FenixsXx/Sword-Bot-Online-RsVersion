# 🤖 Sword Bot Online RS — Guida Completa di Installazione e Avvio

Benvenuto nella guida ufficiale per configurare, compilare ed eseguire **Sword Bot Online RS**, il bot per Discord sviluppato in Rust con pannello di controllo web integrato.

---

## 📋 Indice
1. [Configurazione delle Variabili d'Ambiente (`.env`)](#1-configurazione-delle-variabili-dambiente-env)
2. [Modalità 1: Compilazione ed esecuzione sul proprio Sistema Operativo (Locale / Sviluppo)](#2-modalità-1-compilazione-ed-esecuzione-sul-proprio-sistema-operativo-locale--sviluppo)
3. [Modalità 2: Compilazione e caricamento su una Macchina Virtuale (VM Linux)](#3-modalità-2-compilazione-e-caricamento-su-una-macchina-virtuale-vm-linux)
4. [Modalità 3: Hosting su piattaforme gratuite per bot Discord (PaaS / Bot-Hosting / Docker)](#4-modalità-3-hosting-su-piattaforme-gratuite-per-bot-discord-paas--bot-hosting--docker)
5. [Comandi utili e gestione dei backup](#5-comandi-utili-e-gestione-dei-backup)

---

## 1. Configurazione delle Variabili d'Ambiente (`.env`)

Il progetto include un file modello denominato `env.example.txt` (facilmente apribile e modificabile con qualsiasi editor di testo anche su macOS). 

Per configurare il bot:
1. Fai una copia del file `env.example.txt` e rinominala in `.env` (il punto iniziale è importante affinché il bot lo riconosca):
   ```bash
   cp env.example.txt .env
   ```
2. Apri il file `.env` appena creato e inserisci i tuoi dati personali (token Discord, ID del server, credenziali OAuth2, ecc.). Troverai spiegazioni dettagliate riga per riga all'interno del file.

---

## 2. Modalità 1: Compilazione ed esecuzione sul proprio Sistema Operativo (Locale / Sviluppo)

Ideale se vuoi eseguire e testare il bot direttamente sul tuo PC (Windows, macOS o Linux).

### Requisiti:
- **Rust** (versione stabile recente, consigliato Rust 1.85+ con Cargo). Installabile da [rustup.rs](https://rustup.rs/).
- **SQLite3** installato nel sistema.

### Passaggi:
1. Apri un terminale nella cartella principale del progetto.
2. Assicurati di aver creato e configurato il file `.env` partendo da `env.example.txt`.
3. Compila ed esegui il bot in modalità di rilascio (`release`), che garantisce le massime prestazioni:
   ```bash
   cargo run --release --manifest-path sword_bot_online_rs/Cargo.toml
   ```
   *(Oppure entra nella cartella `sword_bot_online_rs` con `cd sword_bot_online_rs` e digita `cargo run --release`)*.

---

## 3. Modalità 2: Compilazione e caricamento su una Macchina Virtuale (VM Linux)

Se possiedi una VPS o una VM Linux (es. Ubuntu/Debian) e desideri far girare il bot stabilmente 24/7:

1. Connettiti via SSH alla VM e installa gli strumenti di compilazione:
   ```bash
   sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev sqlite3
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source "$HOME/.cargo/env"
   ```
2. Clona o trasferisci i file del progetto sulla VM.
3. Copia `env.example.txt` in `.env` nella cartella principale e configuralo con i tuoi dati.
4. Per mantenere il bot attivo in background in modo sicuro, puoi configurarlo come servizio **systemd** creando un file `/etc/systemd/system/swordbot.service`:
   ```ini
   [Unit]
   Description=Sword Bot Online RS
   After=network.target

   [Service]
   Type=simple
   User=tuo_utente
   WorkingDirectory=/percorso/del/progetto/sword_bot_online_rs
   ExecStart=/home/tuo_utente/.cargo/bin/cargo run --release
   Restart=always
   EnvironmentFile=/percorso/del/progetto/.env

   [Install]
   WantedBy=multi-user.target
   ```
5. Abilita e avvia il servizio:
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable swordbot
   sudo systemctl start swordbot
   ```

---

## 4. Modalità 3: Hosting su piattaforme gratuite per bot Discord (PaaS / Bot-Hosting / Docker)

Molti pannelli di hosting per bot Discord (es. pannelli Pterodactyl, Bot-Hosting.net, Railway, Fly.io) richiedono di caricare i file sorgente o un archivio compresso (`deploy_bundle.tar.gz`).

### Linee guida per l'hosting cloud:
1. **Compilazione ed Esecuzione**: L'hosting deve disporre di un ambiente Rust stable recente (il progetto utilizza Rust edition 2024). Il comando di avvio configurato nel pannello deve essere:
   ```bash
   cargo run --release
   ```
   *(Se il pannello richiede una fase di build separata, inserisci: `cargo build --release`)*.
2. **Variabili d'Ambiente**: Non inserire mai token o credenziali nei file caricati nello ZIP. Puoi usare `env.example.txt` come promemoria delle variabili necessarie, inserendo poi tutte le chiavi (come `DISCORD_TOKEN`, `DISCORD_OAUTH_CLIENT_SECRET`, ecc.) direttamente nella sezione **Variabili d'Ambiente / Segreti** del pannello di hosting.
3. **Persistenza dei Dati (Database)**: Assicurati che il database SQLite non venga cancellato ai riavvi. Configura il percorso nel pannello puntando alla cartella persistente del container:
   ```env
   DB_PATH=/home/container/sword_bot.db
   BACKUP_DIR=/home/container/backups
   ```
4. **Porta Web**: Se il pannello assegna una porta specifica per il traffico web, assicurati di mappare la variabile:
   ```env
   EVENT_OVERLAY_PORT=<porta_assegnata_dal_pannello>
   EVENT_OVERLAY_HOST=0.0.0.0
   ```

---

## 5. Comandi utili e gestione dei backup

- **Ripristino di un backup del database**:
  Il bot include uno strumento da riga di comando per ripristinare i backup in sicurezza:
  ```bash
  cargo run --bin restore_backup --manifest-path sword_bot_online_rs/Cargo.toml
  ```
- **Controllo dello stato (Health Check)**:
  Il server web integrato espone un endpoint `/health` che restituisce lo stato operativo del bot e dei servizi connessi.
