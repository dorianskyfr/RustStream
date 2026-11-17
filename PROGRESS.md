# Avancement de RustStream

Dernière mise à jour : 2026-11-17. Ce fichier permet de reprendre le développement sans perdre les décisions.

## État actuel

Phases 0 et 1 terminées ; phase 2 fonctionnelle de bout en bout **en boucle locale**, avec un encodeur logiciel. Pas encore testé entre deux PC physiques.

## Décisions structurantes

| Sujet | Décision | Référence |
|---|---|---|
| Interface | Slint 1.19 (licence Royalty-free, attribution dans À propos) | docs/adr/0001-ui-framework.md |
| Transport | QUIC (Quinn 0.11), TLS 1.3 ; contrôle et entrées en flux fiables, vidéo en datagrammes | ARCHITECTURE.md |
| Identité | Certificat ECDSA P-256 auto-signé par appareil, empreinte SHA-256, clé chiffrée DPAPI | SECURITY.md |
| Appairage | Code SAS à 6 chiffres, confirmé sur les deux appareils ; un client qui ne connaît pas l'hôte impose toujours l'appairage | SECURITY.md |
| Capture | DXGI Desktop Duplication (copie GPU→CPU tant que l'encodage est logiciel) | ARCHITECTURE.md |
| Codec | OpenH264 logiciel en repli explicite ; encodeurs matériels à venir | docs/LICENSES.md (brevets) |
| Licence | MIT OR Apache-2.0 | LICENSE-* |

## Terminé et testé

- Workspace Cargo (11 crates), CI GitHub Actions (fmt, clippy, tests, build release, benchmark de fumée, cargo-deny).
- Protocole v1 : messages validés, encadrement borné, fragmentation/réassemblage vidéo (tests de perte, désordre, doublons, éviction, entrées malformées).
- Sécurité : identité persistante chiffrée DPAPI, vérificateurs TLS par empreinte, SAS, liste de confiance, révocation, limitation des tentatives.
- Réseau : endpoints QUIC, découverte LAN par balises UDP.
- Capture DXGI réelle (vérifiée sur Intel Iris Xe, 1920×1080 à 60 Hz, conscience DPI par écran).
- Encodage/décodage H.264 OpenH264 (aller-retour testé : erreur moyenne bornée, image clé forcée, changement de taille).
- Entrées : injection `SendInput` avec relâchement garanti, hook clavier client, projection multi-écrans (tests unitaires).
- Moteur de session : tests d'intégration en boucle locale avec QUIC/TLS/H.264 réels : appairage, reconnexion épinglée, révocation, refus, imposteur, appairage désactivé, une seule session à la fois, confirmation locale obligatoire du code.
- CLI : `info`, `host`, `connect`, `trusted`, `bench-loopback`, `bench-capture`.

## En cours

- Application de bureau Slint : interface écrite (4 sections, appairage, session plein écran, statistiques) ; compilation et essai manuel en cours.
- Revue Codex du code sécurité / réseau / concurrence.

## Restant (par phase)

- **Phase 2** : essai réel entre deux PC Windows ; incrustation du curseur distant ; mise à l'échelle de la résolution.
- **Phase 3** : encodeurs matériels (Quick Sync / Media Foundation, NVENC, AMF), décodage matériel, audio WASAPI + Opus, manettes, souris relative et verrouillage du curseur, débit adaptatif, mesure du temps d'affichage.
- **Phase 4** : audit des pilotes USB (usbipd-win, usbip-win2), redirection USB encapsulée dans QUIC.
- **Phase 5** : traversée NAT, relais auto-hébergeable, reconnexion automatique.
- **Phases 6-7** : profilage, rendu par texture GPU partagée, installateur, mises à jour, audit de sécurité.

## Blocages

- **NASM absent** sur la machine de développement : OpenH264 est compilé sans SIMD (encodage 1080p ≈ 51 ms p50 sur le bureau réel). L'installer demande des droits administrateur (en attente d'accord).
- Un seul PC disponible : les essais entre deux machines restent à faire.

## Prochaine tâche prioritaire

1. Essai manuel de l'application de bureau (hôte + client sur la même machine, puis deux PC).
2. Encodeur matériel Quick Sync via Media Foundation (candidat à la délégation à Codex).

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

2026-11-17, Windows 11, Rust 1.100.0 :
- `cargo fmt --all -- --check` : OK.
- `cargo clippy --workspace --all-targets` : 0 avertissement.
- `cargo test --workspace` : **51 tests, 51 réussis** (protocol 13, security 11, input 5, intégration en boucle locale 5, core 4, codec 3, telemetry 3, platform-windows 3, capture 2, network 2).
- Application de bureau : lancée ; Accueil, Périphériques, Performances et Paramètres affichés et navigables par automatisation UI (arbre d'accessibilité exposé). Session vidéo depuis l'interface : **pas encore essayée**.
