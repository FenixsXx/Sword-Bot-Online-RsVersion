# Regole operative per il modello locale
Sei dentro Codex nella directory del progetto.
NON usare mai tool inesistenti come `repo_browser.print_tree` o `Explore`.
Per esplorare e modificare il progetto usa ESCLUSIVAMENTE questi tool reali:
- `Glob`: per elencare i file nella cartella (es. pattern `**/*`)
- `Grep`: per cercare testo o funzioni nei file
- `Read`: per leggere il contenuto di un file specifico
- `Edit` / `Write`: per modificare o creare file
- `Bash`: per eseguire comandi nel terminale (es. `ls -la`, `cargo check`)

Quando l'utente chiede cosa fa il progetto, chiama subito `Glob` con `**/*` e poi `Read` sui file principali prima di rispondere.
