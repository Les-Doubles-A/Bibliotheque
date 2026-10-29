# Bibliotheque (Rust Edition)

Application Windows native ultra-rapide ecrite en **Rust** (`egui`/`eframe` + Win32 + `winsqlite3` + `winhttp`) qui repertorie les executables (`.exe`) et raccourcis d'applications (`.lnk`) du PC.

## Fonctionnalites & Optimisations

- **100 % Rust natif** : binaire autonome leger (`dist\Bibliotheque.exe`) avec demarrage instantane, sans dependance Python.
- **Scanner multi-thread work-stealing (`crossbeam-deque`)** : parcours parallele sur tous les coeurs CPU avec filtrage sans allocation des dossiers systeme (`winsxs`, `installer`, `$recycle.bin`, `node_modules`, `.git`, etc.) et des extensions.
- **Base SQLite native (`winsqlite3.dll`) en mode WAL** : transactions atomiques preparees ultra-rapides et compatibilite totale avec `%LOCALAPPDATA%\Bibliotheque\library.db`.
- **Index en memoire avec recherche SIMD (`memchr::memmem`)** : filtrage instantane (`< 0.1 ms`) par nom, chemin ou categorie a chaque frappe.
- **Table virtualisee & icones Shell asynchrones (pile LIFO)** : seules les lignes visibles a l'ecran sont rendues ; les vraies icones Windows (`IShellItemImageFactory`, `IShellLinkW`, `SHGetFileInfoW`) sont extraites sur un pool de 4 threads COM sans jamais bloquer l'interface.
- **Verification et mise a jour automatique (`winhttp.dll`)** : verifie en arriere-plan au demarrage si une nouvelle version est disponible sur GitHub (`Les-Doubles-A/Bibliotheque`), propose la mise a jour et remplace automatiquement l'executable avant de relancer l'application.
- **Theme clair / sombre automatique** : synchronise en temps reel avec le registre Windows (`AppsUseLightTheme`).

## Compilation

```bat
build.bat
```
ou directement via Cargo :
```bat
cargo build --release
```
