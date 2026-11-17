# Benchmarks

Toutes les valeurs ci-dessous ont été **mesurées** ; aucune latence n'est garantie. Résultats bruts : [`docs/benchmarks/`](docs/benchmarks/).

## Méthodologie

| Commande | Ce qui est mesuré |
|---|---|
| `ruststream-cli bench-loopback --size WxH --fps N --bitrate KBPS --seconds S` | Hôte + client dans le même processus via 127.0.0.1, mire synthétique, QUIC/TLS réels. Décodage et « capture → décodé » **par image** ; encodage en médianes par seconde. 2 s de préchauffage exclues. |
| `ruststream-cli bench-capture --display N --seconds S` | Capture DXGI réelle (attente de la prochaine mise à jour du bureau + copie GPU→CPU) et encodage logiciel sur le contenu réel de l'écran. |

- Toujours en `--release`.
- « Capture → décodé » utilise l'horloge murale : valable en boucle locale (même horloge) ; entre deux PC, il faut des horloges synchronisées.
- Le temps d'affichage (présentation Slint) n'est pas encore mesuré.
- Percentiles « nearest-rank » exacts (crate `telemetry`).

## Résultats

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
