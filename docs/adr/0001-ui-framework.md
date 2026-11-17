# ADR 0001 — Framework d'interface : Slint

- Statut : accepté (2026-11-16)
- Contexte : l'interface doit être native, très épurée, accessible (clavier, lecteurs d'écran), peu gourmande au repos et capable d'afficher un flux vidéo sans ralentir la session.

## Options comparées

| Critère | Slint 1.19 | egui | Tauri/Web |
|---|---|---|---|
| Rendu | Retenu (redessine seulement ce qui change) | Immédiat (redessine chaque frame tant qu'il y a interaction/animation) | WebView |
| CPU au repos | Quasi nul | Faible mais boucle de rendu fréquente | WebView résidente (dizaines de Mo) |
| Design premium | Langage déclaratif, thèmes, animations | Style « outil », personnalisation laborieuse | Excellent (CSS) |
| Accessibilité | AccessKit, rôles/labels déclaratifs | AccessKit | Navigateur |
| Intégration vidéo | `Image` depuis `SharedPixelBuffer` ; textures GPU empruntées possibles (OpenGL/WGPU) | Textures faciles | Difficile sans copie |
| Licence | GPLv3 / Royalty-free (attribution) / commerciale | MIT/Apache | MIT/Apache |
| Dépendances | Moyennes | Légères | Lourdes (WebView2) |

## Décision

Slint, sous la **licence Royalty-free 2.0** (applications de bureau) : obligation d'attribution, satisfaite par le widget `AboutSlint` dans Paramètres › À propos. Compatible avec la licence MIT/Apache-2.0 du projet.

## Conséquences

- Le flux vidéo est actuellement copié dans un `SharedPixelBuffer` puis téléversé par le moteur de rendu de Slint (2 copies CPU + 1 envoi GPU par image). Acceptable pour le MVP ; la phase 6 remplacera ce chemin par une texture D3D11/WGPU partagée avec le décodeur matériel.
- Le clavier est capturé par un hook bas niveau Windows (codes matériels), pas par les événements texte de Slint.
