# Bibliotheque (Rust Edition)

Dépôt GitHub : **https://github.com/Les-Doubles-A/Bibliotheque**

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

<!-- installeur-release -->
## Installation

Téléchargez **`Bibliotheque-Setup-2.0.0.exe`** depuis la [dernière release](https://github.com/Les-Doubles-A/Bibliotheque/releases/latest), puis lancez-le.

L'installation se fait pour l'utilisateur courant, sans droits administrateur (par défaut dans `%LOCALAPPDATA%\Programs\Bibliotheque`), avec un raccourci dans le menu Démarrer et une désinstallation depuis les Paramètres Windows.

L'installeur n'est pas signé : si Windows SmartScreen l'affiche, cliquez sur « Informations complémentaires » puis « Exécuter quand même ».

### Recréer l'installeur

Compiler l'application (`cargo build --release`), puis lancer [Inno Setup 6](https://jrsoftware.org/isinfo.php) : `iscc installer\Bibliotheque.iss`. Le setup est créé dans `installer\Output\`.
