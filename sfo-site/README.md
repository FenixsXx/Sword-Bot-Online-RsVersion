# Sword Friends Online · Staff Hub

Dashboard staff SFO servita dal bot Discord su `/staff/`, con autenticazione
OAuth Discord, API same-origin e report salvati su Supabase.

Le schede riepilogative mostrano gli eventi, i nuovi membri e la durata media
del mese corrente fino a oggi, confrontati con il totale dell'intero mese
precedente. I dati sono calcolati dai report letti da Supabase.

La panoramica confronta le categorie mostrando direttamente la media delle
presenze per evento in barre ordinate dalla media più alta alla più bassa,
senza trasformarle in quote percentuali. Il grafico settimanale mostra le
presenze e i nuovi membri del singolo giorno quando si passa sui punti (o li si
seleziona da tastiera).

## Configurazione Supabase

1. Crea un progetto gratuito su [Supabase](https://supabase.com/).
2. In **SQL Editor**, esegui tutto il contenuto di
   [supabase-schema.sql](./supabase-schema.sql).
3. In **Project Settings > API**, copia il **Project URL** e la chiave
   `service_role`. Salvali solo come variabili segrete nell'hosting del bot:
   `SUPABASE_URL` e `SUPABASE_SERVICE_ROLE_KEY`. La chiave service-role non va
   mai inclusa nel frontend, in un file versionato o condivisa pubblicamente.

Il frontend non usa Supabase Auth né accede direttamente al database. Il bot
serve le API e inoltra le richieste al database usando la chiave configurata
nell'ambiente server.

## Avvio locale

Per avviare il frontend in sviluppo:

```sh
npm install
npm run dev
```

Per compilare gli asset di produzione in `dist/`:

```sh
npm run build
```

Il vecchio [server.js](./server.js) resta disponibile come riferimento per il
precedente setup locale; il flusso di autenticazione e le API staff richiedono
il bot SBO.

La sola build statica non include l'autenticazione OAuth né le API staff:
ospita il sito tramite il bot SBO per abilitare tutte le funzioni.

La durata è limitata a intervalli di 15 minuti. Nome evento, responsabile e
persona che registra la clip propongono valori già usati tramite quick-select
HTML. Il campo “clip registrata” riguarda la clip per i social, mentre “flop”
segnala un evento con andamento negativo.

## Hosting tramite Sword Bot Online

Il backend del bot è implementato in Rust in
[`sword_bot_online_rs/`](../sword_bot_online_rs/). Per avviarlo dalla cartella
principale del progetto usa:

```sh
cargo run --manifest-path sword_bot_online_rs/Cargo.toml
```

Per compilare il bot in modalità produzione:

```sh
cargo build --release --manifest-path sword_bot_online_rs/Cargo.toml
```

Il bot SBO serve questo frontend su `/staff/`. In sviluppo esegui
`npm run build`; per default il bot serve `sfo-site/dist`. In hosting configura
`SFO_SITE_DIR` solo se vuoi usare una diversa cartella di build. La build Vite
usa il prefisso `/staff/` e il bot serve i bundle compilati da
`sfo-site/dist/assets/` sotto `/staff/assets/`. Dopo ogni modifica al sito,
ricompila e assicurati che `SFO_SITE_DIR` punti alla cartella `dist` aggiornata.
Configura nel pannello hosting:

```text
DISCORD_OAUTH_CLIENT_ID=<client-id-app-discord>
DISCORD_OAUTH_CLIENT_SECRET=<client-secret-app-discord>
DISCORD_OAUTH_REDIRECT_URI=https://<dominio-bot>/staff/auth/callback
SUPABASE_URL=https://<project-ref>.supabase.co
SUPABASE_SERVICE_ROLE_KEY=<service-role-key>
```

In Discord Developer Portal, aggiungi lo stesso URL callback OAuth e abilita
il bot sul server identificato da `GUILD_ID`. Il proprietario e i membri con
permesso Discord **Amministratore** possono accedere fin dall'inizio; da
**Gestione > Server** gli amministratori possono poi autorizzare i ruoli staff.
Non serve configurare una password o un account staff Supabase.

Scegli il canale report dalla dashboard (**Gestione > Server**). I report
vengono creati, letti, modificati e pubblicati dal backend del bot; il browser
non riceve mai la chiave service-role.

La sezione **Gestione** del sito permette di scegliere il canale di benvenuto,
attivare/disattivare il saluto, modificare titolo/testo/GIF HTTPS e inviare una
prova nel canale selezionato. I segnaposto disponibili nel testo sono `{user}`,
`{server}`, `{ordinal}` e `{member_count}`; il bot aggiunge avatar del nuovo
membro e footer “Buona permanenza!”.

Nella scheda **Sondaggi** scegli il canale e gli slot community, crea o modifica
sondaggi (nome, emoji, colore, ordine, tipo community e stato), aggiungi o
rimuovi opzioni, invia un'anteprima temporanea e pubblica i sondaggi attivi.
Le operazioni amministrative sono disponibili agli amministratori Discord e
vengono eseguite dal bot, non direttamente dal browser su Discord.

Le nuove proposte community passano dalla revisione staff: usa `/proposte` per
consultare gli ID e `/proposta revisiona`, `/proposta approva` o
`/proposta rifiuta` per aggiornarne lo stato. Le proposte approvate diventano
visibili per il supporto e possono essere inserite nel ballottaggio con
`/scegli`; agli autori vengono inviate notifiche private secondo le preferenze
DM impostate. I sondaggi attivi si chiudono automaticamente sette giorni dopo
la pubblicazione; disattivandoli e riattivandoli dalla Gestione si avvia un
nuovo intervallo di sette giorni.

## Backup cifrati su Cloudflare R2

SBO crea backup SQLite verificati ogni giorno alle 03:15 (`Europe/Rome`) e uno
settimanale il lunedì. I file locali restano in `BACKUP_DIR`; se imposti tutte
le variabili seguenti, ogni snapshot viene anche cifrato con Fernet e caricato
su R2 tramite API S3:

```text
BACKUP_S3_BUCKET=<nome-bucket>
BACKUP_S3_ENDPOINT_URL=https://<account-id>.r2.cloudflarestorage.com
BACKUP_S3_ACCESS_KEY_ID=<access-key-r2>
BACKUP_S3_SECRET_ACCESS_KEY=<secret-access-key-r2>
BACKUP_S3_REGION=auto
BACKUP_S3_PREFIX=sfo-backups
BACKUP_ENCRYPTION_KEY=<chiave-fernet>
```

Genera la chiave localmente con `openssl rand -base64 32 | tr '+/' '-_'` e
conservala come segreto di hosting separato dalle credenziali R2. Senza una di
queste impostazioni, SBO mantiene solo i backup locali. Applica anche una
regola di lifecycle R2 per limitare la retention degli oggetti.

Per ripristinare un oggetto `.db.enc`, scaricalo sul server, arresta SBO e
imposta `BACKUP_ENCRYPTION_KEY` nell'ambiente prima di eseguire:

```sh
cargo run --manifest-path sword_bot_online_rs/Cargo.toml --bin restore_backup -- backups/daily/daily-AAAA-MM-GG.db.enc --database sword_bot.db --preview
cargo run --manifest-path sword_bot_online_rs/Cargo.toml --bin restore_backup -- backups/daily/daily-AAAA-MM-GG.db.enc --database sword_bot.db
```

`--preview` verifica integrità, cifratura e tabelle, confronta i conteggi delle
righe con il database attuale e non modifica alcun file. Esegui il ripristino
solo dopo aver controllato l'anteprima.
