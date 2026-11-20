# Benchmarks

Toutes les valeurs ci-dessous ont été **mesurées** ; aucune latence n'est garantie. Résultats bruts : [`docs/benchmarks/`](docs/benchmarks/).

## Méthodologie

| Commande | Ce qui est mesuré |
|---|---|
| `ruststream-cli bench-loopback --size WxH --fps N --bitrate KBPS --seconds S --encoder auto\|hardware\|software` | Hôte + client dans le même processus via 127.0.0.1, mire synthétique, QUIC/TLS réels. Décodage et « capture → décodé » **par image** ; encodage en médianes par seconde. 2 s de préchauffage exclues. |
| `ruststream-cli bench-capture --display N --seconds S` | Capture DXGI réelle (attente de la prochaine mise à jour du bureau + copie GPU→CPU) et encodage Auto sur le contenu réel de l'écran ; nom du backend affiché. |

- Toujours en `--release`.
- « Capture → décodé » utilise l'horloge murale : valable en boucle locale (même horloge) ; entre deux PC, il faut des horloges synchronisées.
- Le temps d'affichage (présentation Slint) n'est pas encore mesuré.
- Percentiles « nearest-rank » exacts (crate `telemetry`).
- L'encodage matériel inclut la conversion CPU BGRA→NV12, la copie dans le tampon MF, le passage au worker MTA et l'attente de la sortie ; ce n'est pas une mesure du seul temps GPU.

## Résultats

### 2026-11-20 — Intel Iris Xe, Media Foundation contre OpenH264

Windows x64, Rust 1.100.0. Deux exécutions successives après la fin des compilations, en release :

```text
cargo run --release -p ruststream-cli -- bench-loopback --size 1920x1080 --fps 60 --seconds 10 --encoder hardware
cargo run --release -p ruststream-cli -- bench-loopback --size 1920x1080 --fps 60 --seconds 10 --encoder software
```

Mire actuelle à dégradés défilants, 20 000 kbps demandés (valeur par défaut), 2 s de préchauffage puis 10 s de mesure. Décodeur OpenH264 dans les deux cas. Extraits de sortie : [matériel](docs/benchmarks/2026-11-20-hardware.txt), [logiciel](docs/benchmarks/2026-11-20-software.txt).

| Mesure | Intel® Quick Sync Video H.264 Encoder MFT | OpenH264 (software) |
|---|---:|---:|
| Encodage p50 (médianes par seconde, n=10) | 15,73 ms | 111,75 ms |
| Encodage p95 (mêmes échantillons) | 21,04 ms | 112,00 ms |
| Décodage p50 / p95 | 20,98 / 25,50 ms | 12,16 / 15,20 ms |
| Capture → décodé p50 / p95 | 44,09 / 50,19 ms | 124,88 / 132,08 ms |
| Images décodées mesurées | 337 | 86 |
| Moyenne des i/s décodées affichée par le benchmark | 35,5 | 8,5 |
| Débit affiché, dernière seconde (pas une moyenne du test) | 11,8 Mbit/s | 2,2 Mbit/s |
| Images abandonnées / paquets perdus affichés | 3 / 0 | 0 / 0 |

L'encodage matériel fonctionne avec OpenH264, mais cette boucle complète ne soutient pas les 60 i/s demandées. Les copies CPU et le décodeur logiciel restent présents ; aucun débit de 20 Mbit/s effectif ni aucune cadence de 60 i/s ne sont garantis. NVIDIA/AMD et les builds non-Windows n'ont pas été exécutés sur cette machine.

Contre-mesure indépendante (même machine, même jour, mêmes commandes, binaire release du worktree) :

| Mesure | Matériel (Quick Sync) | Logiciel (OpenH264) |
|---|---:|---:|
| Encodage p50 / p95 | 22,73 / 23,16 ms | 210,29 / 212,96 ms |
| Décodage p50 / p95 | 23,23 / 26,32 ms | 24,04 / 28,42 ms |
| Capture → décodé p50 / p95 | 46,79 / 51,52 ms | 240,67 / 286,00 ms |
| i/s décodées (moyenne) | 31,8 | 4,4 |
| Débit, dernière seconde | 10,7 Mbit/s | 1,3 Mbit/s |

Les valeurs absolues varient d'une exécution à l'autre (charge et état d'alimentation du portable), mais le rapport reste net : encodage 7 à 9 fois plus rapide en matériel. Le décodage OpenH264 (≈ 23 ms en 1080p, sans SIMD faute de NASM) devient le goulot d'étranglement et limite la boucle à environ 30-40 i/s.

### 2026-11-16 — Portable Intel Iris Xe (Windows 11), OpenH264 **sans SIMD** (NASM absent)

Build : OpenH264 compilé depuis les sources **sans assembleur** (NASM non installé), donc nettement plus lent que la normale. À refaire avec NASM.

`bench-capture`, écran 1920×1080 à 60 Hz, contenu réel du bureau, 5 s :

| Étape | p50 | p95 | p99 | max | n |
|---|---|---|---|---|---|
| Acquisition + copie GPU→CPU | 4,66 ms | 6,09 ms | 25,06 ms | 25,06 ms | 79 |
| Encodage OpenH264 (logiciel, C pur) | 50,87 ms | 102,16 ms | 168,74 ms | 168,74 ms | 79 |

`bench-loopback`, 1920×1080, 60 i/s demandés, 20 Mbit/s, 10 s, **ancienne mire** (bruit XOR : chaque pixel change à chaque image, cas pathologique, depuis remplacée par des dégradés défilants) :

| Étape | p50 | p95 | p99 | max |
|---|---|---|---|---|
| Encodage (médianes par seconde) | 244 ms | 249 ms | 249 ms | 249 ms |
| Décodage | 39 ms | 50 ms | 52 ms | 52 ms |
| Capture → décodé | 292 ms | 364 ms | 374 ms | 374 ms |

3,8 i/s décodées en moyenne ; 0 paquet perdu.

**Conclusion** : la capture DXGI n'est pas le goulot d'étranglement ; l'encodage logiciel l'est. Prochaines mesures : OpenH264 avec NASM, puis encodeurs matériels (Quick Sync sur cette machine).

## Régressions observées

Aucune référence antérieure.
