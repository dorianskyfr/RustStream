# Sécurité de RustStream

## Modèle de menace

| Menace | Protection |
|---|---|
| Écoute du réseau | QUIC + TLS 1.3 sur tout le trafic (vidéo, entrées, contrôle) |
| Connexion d'un appareil inconnu | Authentification mutuelle par certificat ; liste de confiance côté hôte ; appairage avec approbation explicite |
| Homme du milieu à l'appairage | Code de 6 chiffres (SAS) dérivé des deux empreintes **et de nombres aléatoires frais échangés par engagement/révélation**, affiché sur les deux écrans ; **les deux utilisateurs doivent confirmer**. Un attaquant ne peut pas choisir le code : 1 chance sur 1 000 000 par tentative, tentatives limitées |
| Usurpation d'un hôte connu | Empreinte de l'hôte épinglée côté client : la poignée TLS échoue si le certificat change |
| Faux hôte annoncé sur le LAN | Les balises de découverte ne sont pas authentifiées et ne servent que d'indice ; un hôte inconnu impose toujours l'appairage avec confirmation locale du code |
| Force brute sur l'appairage | 5 échecs max par adresse IP et par minute ; délai d'approbation 60 s |
| Messages malformés | Taille bornée avant allocation, rejet des octets résiduels, validation sémantique de chaque message, en-têtes vidéo vérifiés |
| Vol de la clé privée | Clé chiffrée par DPAPI (liée au compte Windows), entropie propre à l'application |
| Contrôle à l'insu de l'utilisateur | Partage désactivé par défaut ; bandeau « Session active » permanent et bouton « Terminer » sur l'hôte |
| Touches bloquées après une coupure | L'injecteur relâche toutes les touches et boutons enfoncés à la fin de la session |

## Identité et appairage

- Chaque installation génère une paire de clés ECDSA P-256 et un certificat auto-signé. L'identité est l'empreinte SHA-256 du certificat ; aucune autorité de certification.
- TLS vérifie la signature de la poignée de main (preuve de possession de la clé). La chaîne de certificats est remplacée par la vérification d'empreinte : épinglage côté client, liste de confiance côté hôte.
- Appairage : le client indique `request_pairing`. L'hôte envoie un engagement `H(empreinte_hôte ‖ Nh)` sur un nombre aléatoire `Nh` ; le client répond avec son nombre `Nc` ; l'hôte révèle `Nh`, que le client vérifie. Le code vaut `H(empreinte_client ‖ empreinte_hôte ‖ Nc ‖ Nh) mod 10⁶`. Un code dérivé seulement des empreintes publiques (version initiale) pouvait être forcé hors ligne par un attaquant générant des certificats : corrigé suite à la revue Codex du 2026-11-17. L'hôte affiche le nom de l'appareil et le code, son utilisateur approuve ; l'utilisateur du client confirme que le code est identique. Aucune image n'est affichée ni aucune entrée envoyée avant les deux confirmations.
- Révocation : Paramètres › Appareils autorisés › Révoquer (ou `ruststream-cli trusted --revoke`). La connexion suivante de cet appareil exige un nouvel appairage.

## Exposition réseau

- Rien n'écoute tant que « Partager » est désactivé (défaut).
- Aucune ouverture automatique de port (pas d'UPnP). La traversée NAT et le relais (phase 5) seront optionnels.
- Aucune télémétrie : les statistiques restent locales.

## Journaux

Les journaux (`tracing`) ne contiennent ni clé, ni contenu d'écran, ni frappe clavier. Les empreintes sont affichées sous forme courte.

## Pilotes (phase 4)

Aucun pilote n'est installé aujourd'hui. Règles qui s'appliqueront : jamais de désactivation de Secure Boot ni de la signature des pilotes, aucun pilote expérimental installé automatiquement, consentement explicite, risques d'écran bleu documentés, procédure de désinstallation et de récupération.

## Signaler une vulnérabilité

Merci d'utiliser les « Security advisories » privés de GitHub sur https://github.com/dorianskyfr/RustStream/security/advisories plutôt qu'une issue publique.

## Limites actuelles

- La clé est protégée par DPAPI du compte utilisateur : un logiciel malveillant exécuté sous ce compte peut la lire.
- Aucun audit de sécurité externe n'a encore été réalisé.
