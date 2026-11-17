# RustStream

Application libre (MIT ou Apache-2.0) de streaming de PC à distance et, à terme, de redirection USB, écrite en Rust pour Windows 10/11 x64. Objectif : une alternative gratuite et open source à Parsec et Shadow PC.

> **Statut : préversion de développement.** Le chemin de bout en bout fonctionne (capture, encodage, réseau chiffré, décodage, affichage, clavier/souris), mais l'encodage est **logiciel** et donc lent. Ne pas utiliser en production. Voir [PROGRESS.md](PROGRESS.md).

## Fonctionnalités disponibles (testées)

- Une seule application : hôte (« Partager cet ordinateur ») et client.
- Connexion chiffrée QUIC + TLS 1.3, authentification mutuelle par certificat d'appareil.
- Appairage sécurisé : code à 6 chiffres affiché sur les deux écrans, confirmé des deux côtés ; révocation des appareils.
- Découverte automatique des ordinateurs du réseau local.
- Capture d'écran DXGI Desktop Duplication, multi-écrans, reprise après UAC/verrouillage, changement de résolution.
- Vidéo H.264 (OpenH264, **logiciel**) en datagrammes QUIC, priorité à l'image la plus récente, récupération après perte par image clé.
- Clavier (codes matériels, disposition de l'hôte), souris absolue, molette, relâchement automatique des touches à la déconnexion.
- Interface Slint épurée, mode sombre/clair, profils Automatique / Performance / Qualité + mode avancé, statistiques en direct.
- Outil en ligne de commande : hôte sans interface, client, benchmarks.

## Pas encore disponible

Encodage/décodage matériel (NVENC, AMF, Quick Sync), HEVC/AV1, audio, manettes, souris relative / verrouillage du curseur, redirection USB, connexions par Internet (NAT, relais), installateur. Feuille de route : [PROGRESS.md](PROGRESS.md).

## Compiler

Prérequis : Rust stable (≥ 1.88), Visual Studio Build Tools (charge « Développement Desktop en C++ »). Recommandé : [NASM](https://www.nasm.us/) dans le PATH — sans lui, OpenH264 est compilé sans optimisations SIMD et l'encodage est plusieurs fois plus lent.

```bash
cargo build --release
```

Binaires : `target/release/ruststream.exe` (application) et `target/release/ruststream-cli.exe`.

## Utiliser

1. Sur le PC à contrôler : ouvrir RustStream, activer **Partager**. Autoriser RustStream dans le pare-feu Windows (réseau privé) si demandé.
2. Sur l'autre PC : l'ordinateur apparaît dans la liste (même réseau) ou **Ajouter un ordinateur** avec son adresse IP.
3. **Se connecter** : un code s'affiche sur les deux écrans. S'ils sont identiques, confirmer sur le client et **Autoriser** sur l'hôte. Les connexions suivantes sont directes.
4. Pendant la session : barre en haut de l'image (plein écran, statistiques, écran distant, déconnexion). **Ctrl+Alt+Maj+Q** rend le clavier à l'ordinateur local.

En ligne de commande :

```bash
ruststream-cli info
ruststream-cli host
ruststream-cli connect 192.168.1.20
ruststream-cli bench-loopback --size 1920x1080 --fps 60 --seconds 10
```

Pour essayer hôte et client sur un même PC, lancer une seconde instance avec son propre dossier de configuration (identité, réglages et appareils autorisés séparés) et un autre port dans ses Paramètres :

```powershell
$env:RUSTSTREAM_CONFIG_DIR = "$env:TEMPuststream-b"; ruststream.exe
```

## Compatibilité

Windows 10/11 x64. Testé sur un seul PC portable (Intel Iris Xe) en boucle locale ; **pas encore testé entre deux PC physiques**.

## Limites connues

- Encodage logiciel : 1080p loin de 60 i/s sur un portable (voir [BENCHMARKS.md](BENCHMARKS.md)).
- Résolution native transmise (pas de mise à l'échelle) ; curseur distant non dessiné (le curseur local reste visible).
- Bureaux HDR non pris en charge.
- Pas d'accès au bureau sécurisé (UAC, Ctrl+Alt+Suppr) : limite volontaire de Windows.

## Documentation

[ARCHITECTURE.md](ARCHITECTURE.md) · [SECURITY.md](SECURITY.md) · [BENCHMARKS.md](BENCHMARKS.md) · [PROGRESS.md](PROGRESS.md) · [docs/LICENSES.md](docs/LICENSES.md) · [docs/adr/](docs/adr/)

## Licence

Au choix : [MIT](LICENSE-MIT) ou [Apache-2.0](LICENSE-APACHE). Interface réalisée avec [Slint](https://slint.dev) (licence Royalty-free, attribution dans Paramètres › À propos).
