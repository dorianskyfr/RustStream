# Architecture de RustStream

## Vue d'ensemble

```
apps/desktop  (Slint, hôte + client)      apps/cli (hôte sans interface, client, benchmarks)
        \                                   /
         └──────────── crates/core ────────┘   moteur de session (hôte, client, réglages)
             │        │        │       │       │
        network   capture   codec   input   telemetry
             │                                  
   protocol, security                platform-windows (DPAPI, GPU, DPI, timers)
```

Dépendances unidirectionnelles : `protocol`, `security` et `telemetry` sont portables et sans code Windows ; tout le code Windows est derrière `cfg(windows)` (`capture::dxgi`, `input::windows`, `platform-windows`).

| Crate | Rôle |
|---|---|
| `protocol` | Messages versionnés (postcard), encadrement des flux, fragmentation/réassemblage vidéo |
| `security` | Identité d'appareil, empreintes, code d'appairage (SAS), liste de confiance, configurations TLS, limitation des tentatives |
| `network` | Endpoints QUIC (Quinn), E/S de messages, envoi vidéo en datagrammes, découverte LAN |
| `capture` | Capture DXGI Desktop Duplication ; mire synthétique pour tests/benchmarks |
| `codec` | Traits encodeur/décodeur ; backend OpenH264 (logiciel) |
| `input` | Injection `SendInput` avec suivi des touches enfoncées ; hook clavier côté client |
| `telemetry` | Percentiles exacts sur fenêtre glissante, débits ; 100 % local |
| `platform-windows` | DPAPI, énumération GPU, nom de machine, conscience DPI, timer 1 ms |
| `core` | Sessions hôte/client, appairage, réglages persistants |

## Transport

Une connexion QUIC (TLS 1.3, ALPN `ruststream/2`, port UDP 47800 par défaut) porte trois canaux séparés par exigence de livraison :

| Canal | Mécanisme QUIC | Contenu |
|---|---|---|
| Contrôle | flux bidirectionnel fiable | `Hello`, appairage, `Welcome`, demandes d'image clé, statistiques, `Goodbye` |
| Entrées | flux unidirectionnel fiable client→hôte | événements clavier/souris (un relâchement de touche ne doit jamais être perdu) |
| Vidéo | datagrammes non fiables | fragments d'images encodées |

Les lecteurs de flux tournent dans des tâches dédiées qui alimentent des canaux : toutes les boucles `select!` sont annulables sans perte de données.

Paramètres de transport : keep-alive 2 s, timeout d'inactivité 10 s, tampons de datagrammes bornés (2 Mo en émission). En congestion, Quinn abandonne les datagrammes les plus anciens : une image périmée est sacrifiée plutôt que de retarder la plus récente.

### Protocole (v1)

Messages de contrôle encodés en postcard, préfixés par une longueur `u32` LE (max 64 Kio, vérifiée avant toute allocation). Chaque message décodé est validé sémantiquement (bornes de résolution, fps, débit, longueur des noms, caractères de contrôle).

Fragment vidéo (en-tête fixe de 18 octets, sans serde sur le chemin critique) :

| Octets | Champ |
|---|---|
| 0 | type (`0x01` = vidéo) |
| 1–4 | `frame_id` u32 |
| 5–6 | index du fragment u16 |
| 7–8 | nombre de fragments u16 (≤ 4096) |
| 9 | drapeaux (bit 0 = image clé) |
| 10–17 | horodatage de capture (µs Unix, horloge de l'hôte) |

Réassemblage : au plus 4 images en cours ; **l'image la plus récente gagne** — dès qu'une image est complète, toute image plus ancienne incomplète est abandonnée. Un trou (`lost_before > 0`) sur une image non clé fait passer le client en attente d'image clé (demande limitée à une toutes les 250 ms).

## Pipeline vidéo (état actuel)

```
Hôte : DXGI AcquireNextFrame → copie GPU→staging → Map → BGRA (CPU)
       → BGRA→I420 (SIMD openh264-rs) → OpenH264 (logiciel) → fragments → datagrammes QUIC
Client : datagrammes → réassemblage → file bornée (3) → OpenH264 → RGBA → SharedPixelBuffer → Slint
```

- **Cadence** : thread `rs-host-video` cadencé au fps négocié (≤ fréquence de l'écran). À chaque échéance, la dernière image disponible est encodée ; une image arrivée trop tôt reste « en attente » et n'est jamais perdue, donc l'état final du bureau est toujours transmis.
- **Images clés** uniquement à la demande (début, perte, erreur de décodage, changement de résolution) : pas d'IDR périodique qui provoquerait des pics de débit.
- **Changement de résolution** : détecté à la capture → `SessionChanged` + image clé.
- **Perte d'accès** (UAC, écran verrouillé, changement de mode) : la duplication est recréée automatiquement.
- **Limites connues** : copie GPU→CPU, encodage logiciel, pas de mise à l'échelle (résolution native transmise), curseur distant non incrusté (le curseur local est visible), HDR non pris en charge.

**Cible** (phases 3/6) : texture DXGI → encodeur matériel (NVENC/AMF/QSV) sans passage par le CPU ; décodage matériel D3D11VA ; affichage par texture partagée.

## Entrées

- Client : clavier capturé par hook `WH_KEYBOARD_LL` **uniquement** quand une fenêtre RustStream est au premier plan et qu'une session est active ; codes matériels (set 1) transmis, donc la disposition du clavier distant s'applique. Ctrl+Alt+Maj+Q rend le clavier au poste local. Souris en coordonnées absolues normalisées (0–65535) sur l'image affichée.
- Hôte : `SendInput` ; les coordonnées absolues sont projetées sur l'écran diffusé dans le bureau virtuel. L'injecteur mémorise touches et boutons enfoncés et les relâche tous à la déconnexion, en cas d'erreur ou sur `ReleaseAll`.
- Limites Windows respectées (jamais contournées) : bureau sécurisé (UAC, Ctrl+Alt+Suppr, écran de verrouillage) et UIPI (fenêtres d'intégrité supérieure).

## Sécurité

Voir [SECURITY.md](SECURITY.md).

## Redirection USB (phase 4, non commencée)

Architecture envisagée, à valider par un audit :
1. Énumération locale (SetupAPI / libusb) ; 2. export du périphérique (usbipd-win, déjà signé) ; 3. transport USB/IP **encapsulé dans la connexion QUIC authentifiée** (jamais de serveur USB/IP exposé) ; 4. côté distant, pilote VHCI (usbip-win2 ou équivalent signé) ; 5. libération et restauration du périphérique. Aucun pilote ne sera installé sans consentement explicite, et la désinstallation sera documentée.
