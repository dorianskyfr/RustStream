# Licences et obligations de redistribution

RustStream est distribué sous **MIT OR Apache-2.0** (`LICENSE-MIT`, `LICENSE-APACHE`).
La vérification automatique des licences des dépendances est faite en CI par `cargo deny` (`deny.toml`).

| Composant | Licence | Obligation |
|---|---|---|
| Slint | Royalty-free 2.0 (choisie parmi GPLv3 / Royalty-free / commerciale) | Attribution visible : widget `AboutSlint` dans Paramètres › À propos. |
| OpenH264 (Cisco, via crate `openh264`) | BSD-2-Clause | Conserver la notice. **Brevets H.264** : la licence de brevets de Cisco ne couvre que les binaires distribués par Cisco. Les builds de développement compilent OpenH264 depuis les sources ; une release publique devra charger la DLL officielle de Cisco (feature `libloading`) ou utiliser les encodeurs matériels. |
| Quinn, rustls, tokio, serde, postcard… | MIT / Apache-2.0 | Notices. |
| ring | Apache-2.0 AND ISC | Notices. |
| windows-rs | MIT / Apache-2.0 | Notices. |

Aucun pilote n'est distribué à ce stade. Les pilotes USB (phase 4) feront l'objet d'un audit dédié (licence, signature, privilèges) avant toute intégration.
