# Avancement de RustStream

Dernière mise à jour : 2026-11-20. Ce fichier permet de reprendre le développement sans perdre les décisions.

## État actuel

Phases 0 et 1 terminées ; phase 2 fonctionnelle de bout en bout **en boucle locale**. Encodeur H.264 matériel Media Foundation ajouté et aller-retour validé sur Intel Iris Xe ; repli OpenH264 disponible. Pas encore testé entre deux PC physiques.

## Décisions structurantes

| Sujet | Décision | Référence |
|---|---|---|
| Interface | Slint 1.19 (licence Royalty-free, attribution dans À propos) | docs/adr/0001-ui-framework.md |
| Transport | QUIC (Quinn 0.11), TLS 1.3 ; contrôle et entrées en flux fiables, vidéo en datagrammes | ARCHITECTURE.md |
| Identité | Certificat ECDSA P-256 auto-signé par appareil, empreinte SHA-256, clé chiffrée DPAPI | SECURITY.md |
| Appairage | Code SAS à 6 chiffres issu d'un échange engagement/révélation, confirmé sur les deux appareils ; un client qui ne connaît pas l'hôte impose toujours l'appairage | SECURITY.md |
| Protocole | Version 2 (ALPN `ruststream/2`) : incompatible avec la v1 depuis l'appairage par engagement | crates/protocol |
| Capture | DXGI Desktop Duplication (copie GPU→CPU ; entrée CPU BGRA pour les deux encodeurs) | ARCHITECTURE.md |
| Codec | H.264 matériel via Media Foundation, Constrained Baseline ; OpenH264 en repli journalisé ; choix Auto/Hardware/Software | crates/codec, docs/LICENSES.md (brevets) |
| Licence | MIT OR Apache-2.0 | LICENSE-* |

## Terminé et testé

- Workspace Cargo (11 crates), CI GitHub Actions (fmt, clippy, tests, build release, benchmark de fumée, cargo-deny).
- Protocole v1 : messages validés, encadrement borné, fragmentation/réassemblage vidéo (tests de perte, désordre, doublons, éviction, entrées malformées).
- Sécurité : identité persistante chiffrée DPAPI, vérificateurs TLS par empreinte, SAS, liste de confiance, révocation, limitation des tentatives.
- Réseau : endpoints QUIC, découverte LAN par balises UDP.
- Capture DXGI réelle (vérifiée sur Intel Iris Xe, 1920×1080 à 60 Hz, conscience DPI par écran).
- Encodage/décodage H.264 OpenH264 (aller-retour testé : erreur moyenne bornée, image clé forcée, changement de taille).
- Encodeur matériel asynchrone Media Foundation : MTA privé, BGRA→NV12 CPU avec tampon réutilisé, CBR, basse latence, sortie Annex-B avec SPS/PPS. Test réellement exécuté sur « Intel® Quick Sync Video H.264 Encoder MFT » : 16 images décodées par OpenH264, erreur moyenne rouge < 12, IDR forcées décodables isolément et passage 320×180→640×360. NVIDIA/AMD non testés.
- Entrées : injection `SendInput` avec relâchement garanti, hook clavier client, projection multi-écrans (tests unitaires).
- Moteur de session : tests d'intégration en boucle locale avec QUIC/TLS/H.264 réels : appairage, reconnexion épinglée, révocation, refus, imposteur, appairage désactivé, une seule session à la fois, confirmation locale obligatoire du code.
- CLI : `info`, `host`, `connect`, `trusted`, `bench-loopback`, `bench-capture`.
- Application de bureau : nouvelle interface (fond marine, cartes en verre, barre latérale à icônes, carte principale avec sélecteur d'ordinateur, débit et latence en direct avec courbes, vignettes d'ordinateurs, barre de titre sombre). Session réelle lancée **depuis l'interface** entre deux instances sur le même PC (`RUSTSTREAM_CONFIG_DIR`) : même code affiché des deux côtés, appairage, flux reçu (≈ 2,5 Mbit/s, aller-retour 0,4 ms en boucle locale), fin de session par « Terminer ».

## En cours

- Essai entre deux PC physiques.

## Restant (par phase)

- **Phase 2** : essai réel entre deux PC Windows ; incrustation du curseur distant ; mise à l'échelle de la résolution ; **écrans pivotés (portrait)** : la rotation DXGI est désormais appliquée à la copie (`crates/capture/src/rotate.rs`, tests unitaires des 4 orientations), mais **pas encore validée sur un vrai écran pivoté**.
- **Phase 3** : validation des MFT NVIDIA/AMD, entrée GPU sans copie CPU et décodage matériel, audio WASAPI + Opus, manettes, souris relative et verrouillage du curseur, débit adaptatif, mesure du temps d'affichage.
- **Phase 4** : audit des pilotes USB (usbipd-win, usbip-win2), redirection USB encapsulée dans QUIC.
- **Phase 5** : traversée NAT, relais auto-hébergeable, reconnexion automatique.
- **Phases 6-7** : profilage, rendu par texture GPU partagée, installateur, mises à jour, audit de sécurité.

## Blocages

- **NASM absent** sur la machine de développement : OpenH264 est compilé sans SIMD (encodage 1080p ≈ 51 ms p50 sur le bureau réel). L'installer demande des droits administrateur (en attente d'accord).
- Un seul PC disponible : les essais entre deux machines restent à faire.

## Prochaine tâche prioritaire

1. Essai entre deux PC physiques (vidéo, entrées, reconnexion).
2. Réduire les copies GPU/CPU et valider Media Foundation sur d'autres GPU.

## Commandes de reproduction

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
cargo build --release
target/release/ruststream-cli.exe bench-loopback --size 1920x1080 --fps 60 --seconds 10
target/release/ruststream-cli.exe bench-capture --seconds 5
```

## Derniers résultats de tests

2026-11-20, Windows x64, Rust 1.100.0 :
- `cargo fmt --all -- --check` : OK.
- `cargo clippy --workspace --all-targets` : 0 avertissement Clippy.
- Codec : **6 tests réussis**, dont le véritable aller-retour matériel Intel (non sauté), IDR forcées et redimensionnement.
- `cargo test --workspace` (session utilisateur normale) : **59 tests, 59 réussis**, dont `dpapi_roundtrip`. Ce test échoue seulement dans le bac à sable de Codex (`CryptProtectData` : « fichier introuvable »), pas sur la machine.
- Boucle 1080p, 60 i/s demandées, 10 s, release, deux séries indépendantes : encodage matériel p50 **15,7 à 22,7 ms** contre **112 à 210 ms** en logiciel ; i/s décodées **32 à 36** contre **4 à 9**. Le décodage OpenH264 (≈ 23 ms) devient le goulot ; pas de 60 i/s soutenues. Voir [BENCHMARKS.md](BENCHMARKS.md).

Historique — 2026-11-17, Windows 11, Rust 1.100.0 :
- `cargo fmt --all -- --check` : OK.
- `cargo clippy --workspace --all-targets` : 0 avertissement.
- `cargo test --workspace` : **56 tests, 56 réussis** (protocol 13, security 13, input 5, intégration en boucle locale 5, core 4, codec 3, telemetry 3, platform-windows 3, desktop 3, capture 2, network 2).
- Revues Codex : 1re revue (SAS forçable hors ligne, image clé, zone d'écran, barre d'outils, tâches de session, saisie CLI) corrigée ; 2e revue (version du protocole, annulation d'appairage par identifiant, réponses anticipées en CLI) corrigée. Limite de 2 passes atteinte sur ce sujet.
- Application de bureau : Accueil, Performances et Paramètres vérifiés par capture de la fenêtre ; session réelle entre deux instances locales réussie depuis l'interface.
