# claude-rotate

[![CI](https://github.com/Dolyyyy/claude-proxy-rotator-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/Dolyyyy/claude-proxy-rotator-rs/actions/workflows/ci.yml)
[![Licence : MIT](https://img.shields.io/badge/licence-MIT-green)](LICENSE)

Proxy auto-hébergé qui permet à **Claude Code** de s'appuyer sur plusieurs
abonnements Claude Max/Pro et de changer de compte automatiquement quand une
fenêtre de quota se remplit. Un panel intégré affiche les jauges de quota, la
consommation par appareil et son équivalent en dollars au tarif de l'API.

Un seul binaire, un seul fichier de configuration, pas de base de données.

Ce dépôt est un portage en Rust de
[claude-rotate](https://github.com/doxaras/claude-rotate) (Python, John
Doxaras). Les formats de `config.json`, `state.json` et `logs/audit.jsonl`
sont compatibles : une installation existante peut changer de binaire sans
rien réécrire.

> **Conditions d'utilisation.** Cet outil automatise le passage d'un compte à
> l'autre parmi des comptes **que vous possédez et payez vous-même**. Les
> limites de chaque compte restent entièrement appliquées par Anthropic.
> Faire tourner des comptes pour dépasser les limites est une zone grise au
> regard des conditions grand public d'Anthropic : utilisation à vos risques,
> et **jamais** pour adosser un service partagé ou commercial à des
> abonnements grand public. Mettre en commun, partager ou revendre un accès
> est une violation des conditions, pas une zone grise. Pour un usage
> commercial ou partagé, utilisez l'API avec facturation à l'usage.

**Sommaire :**
[Fonctionnement](#fonctionnement) ·
[Installation](#installation) ·
[Démarrer le serveur](#démarrer-le-serveur) ·
[Jetons de compte](#jetons-de-compte-claude-setup-token) ·
[Appareils](#configurer-un-appareil) ·
[Réseau](#réseau) ·
[Panel et endpoints](#panel-et-endpoints) ·
[Configuration](#configuration-configjson) ·
[Rotation](#politique-de-rotation) ·
[Service](#lancer-comme-service) ·
[Dépannage](#dépannage) ·
[Sécurité](#sécurité) ·
[Différences avec l'original](#différences-avec-loriginal-python) ·
[Crédits](#crédits-et-licence)

## Fonctionnement

Claude Code respecte `ANTHROPIC_BASE_URL`, et `claude setup-token` produit
pour chaque compte un jeton OAuth valable environ un an. Le proxy détient ces
jetons ; les appareils ne détiennent qu'une *clé d'appareil* interne,
révocable individuellement.

À chaque requête, le proxy :

1. authentifie l'appareil (clé d'appareil vers nom d'appareil) ;
2. remplace l'en-tête `Authorization` par le jeton du compte actif, sans
   toucher aux autres en-têtes du client ;
3. relaie la requête vers `api.anthropic.com` de façon transparente, flux SSE
   compris ;
4. lit les en-têtes de quota `anthropic-ratelimit-unified-*` de la réponse et
   applique la politique de rotation (seuil atteint ou 429 de quota : rotation ;
   429 de débit : temporisation sur le même compte ; tous les comptes épuisés :
   attente optionnelle jusqu'à la réinitialisation) ;
5. journalise l'usage (appareil, modèle, compte, tokens) dans
   `logs/audit.jsonl`.

Une seule décision de rotation est prise à la fois : quand cent requêtes en
vol reçoivent ensemble un 429 de quota, il y a un seul changement de compte et
chaque requête est rejouée une fois sur le nouveau compte.

Le serveur est conçu pour tourner en **un seul processus** : l'état (compte
actif, télémétrie) vit en mémoire et dans `state.json`.

## Installation

### Depuis les sources

Rust 1.88 ou plus récent :

```bash
cargo install --git https://github.com/Dolyyyy/claude-proxy-rotator-rs --locked
```

### Binaires précompilés

Chaque version publie des binaires sur la page
[Releases](https://github.com/Dolyyyy/claude-proxy-rotator-rs/releases) :
Linux x86_64 et aarch64, Windows x86_64, macOS x86_64 et Apple Silicon.
Décompressez l'archive et placez `claude-rotate` dans le `PATH`
(par exemple `/usr/local/bin`).

### Docker

L'image est publiée sur `ghcr.io/dolyyyy/claude-proxy-rotator-rs`. Toutes les
données (configuration, jetons, état, journaux) vivent dans le volume `/data` :

```bash
docker volume create claude-rotate
alias cr='docker run --rm -it -v claude-rotate:/data ghcr.io/dolyyyy/claude-proxy-rotator-rs'
cr init
cr add-account acct-1
cr add-device mon-portable
```

Dans un conteneur, le proxy doit écouter sur toutes les interfaces : passez
`"bind": "0.0.0.0"` dans `config.json` (la valeur par défaut `127.0.0.1` le
rendrait injoignable depuis l'hôte), puis publiez le port **sur une interface
privée** :

```bash
docker run -d --name claude-rotate --restart unless-stopped \
  -v claude-rotate:/data -p 127.0.0.1:8484:8484 \
  ghcr.io/dolyyyy/claude-proxy-rotator-rs
```

Remplacez `127.0.0.1` par l'adresse LAN ou Tailscale du serveur selon le
[réseau](#réseau) choisi. Le conteneur tourne sous un utilisateur non root.

## Démarrer le serveur

```bash
mkdir ~/claude-rotate && cd ~/claude-rotate
claude-rotate init                      # config.json, tokens/, logs/
claude-rotate add-account acct-1        # jeton du compte 1 (claude setup-token)
claude-rotate add-account acct-2        # jeton du compte 2
claude-rotate add-device mon-portable   # affiche les variables pour cet appareil
claude-rotate                           # équivaut à : claude-rotate serve
```

Toutes les commandes travaillent dans le répertoire courant ; `--root <DIR>`
ou la variable `CLAUDE_ROTATE_ROOT` en désignent un autre.

| Commande | Rôle |
|---|---|
| `serve` (défaut) | lance le proxy |
| `init` | crée `config.json` depuis l'exemple, `tokens/` et `logs/` ; n'écrase rien |
| `add-account <nom>` | enregistre un jeton dans `tokens/<nom>.token` et déclare le compte |
| `add-device <nom>` | génère une clé d'appareil et affiche les variables à poser |
| `remove-device <nom>` | révoque la clé d'un appareil |
| `onboard <user> <device>` | parcours d'équipe, voir [ONBOARDING.md](ONBOARDING.md) |

`add-account` lit le jeton en saisie masquée. Si l'entrée standard n'est pas
un terminal, il lit une ligne sur stdin, ce qui permet de l'utiliser en
script :

```bash
printf '%s\n' "$TOKEN" | claude-rotate add-account acct-3
```

La configuration est lue une fois au démarrage : après `add-account`,
`add-device` ou `remove-device`, redémarrez le serveur.

Le niveau de journalisation se règle avec `RUST_LOG` (défaut :
`claude_rotate=info`, par exemple `RUST_LOG=claude_rotate=debug`).

## Jetons de compte (`claude setup-token`)

Chaque abonnement fournit un jeton OAuth longue durée :

```bash
claude setup-token
```

- La commande ouvre le même parcours OAuth que `/login`. Si aucun navigateur
  ne peut joindre le callback local (serveur sans écran, session SSH), elle
  propose de coller un code : on peut donc produire les jetons sur le serveur
  lui-même.
- Elle affiche un jeton `sk-ant-oat01-…` sans l'enregistrer : collez-le
  directement dans `claude-rotate add-account <nom>`.
- Le jeton est valable environ un an et nécessite un abonnement Claude
  (Pro, Max, Team ou Enterprise).

**Plusieurs comptes depuis une même machine.** Le jeton appartient au compte
qui termine le parcours OAuth dans le navigateur, pas à celui de la session
CLI en cours (comportement constaté, non documenté). Deux méthodes :

1. un profil de navigateur (ou une fenêtre privée) connecté à claude.ai par
   compte : lancez `claude setup-token` et terminez le parcours dans le profil
   du compte voulu ;
2. `CLAUDE_CONFIG_DIR=~/.claude-acct2 claude setup-token`, qui isole
   complètement la session CLI.

Après l'ajout d'un compte, faites passer une requête et consultez
`/rotate/status` : un jeton produit sur le mauvais compte se voit tout de
suite à la jauge qui bouge.

**Révocation.** Il n'existe pas de commande pour lister ou révoquer les jetons
`setup-token` : la révocation se fait depuis les paramètres de claude.ai.
Un jeton mort se manifeste par des 401 renvoyés par Anthropic pour ce compte.

## Configurer un appareil

`add-device` affiche les deux variables à poser sur la machine cliente ; c'est
toute l'installation côté client.

bash / zsh (`~/.bashrc`, `~/.zshrc`) :

```bash
export ANTHROPIC_BASE_URL=http://<serveur>:8484
export CLAUDE_CODE_OAUTH_TOKEN=<clé d'appareil>
```

PowerShell (`$PROFILE`) :

```powershell
$env:ANTHROPIC_BASE_URL = "http://<serveur>:8484"
$env:CLAUDE_CODE_OAUTH_TOKEN = "<clé d'appareil>"
```

Pour un réglage permanent sous Windows sans passer par le profil :
`[Environment]::SetEnvironmentVariable("ANTHROPIC_BASE_URL", "http://<serveur>:8484", "User")`
(idem pour `CLAUDE_CODE_OAUTH_TOKEN`).

Lancez ensuite `claude` normalement, sans connexion Anthropic. L'extension
VS Code lit les mêmes variables : lancez VS Code depuis un shell qui les
définit, ou posez-les au niveau utilisateur.

Si `public_url` est renseigné dans `config.json`, les commandes l'utilisent
pour afficher l'URL exacte ; sinon elles affichent `http://<this-host>:8484`
à compléter.

## Réseau

Le proxy parle HTTP en clair et les clés d'appareil sont des secrets au
porteur : le transport entre appareils et serveur doit être privé.

- **Réseau local de confiance** : `"bind"` sur l'adresse LAN du serveur (ou
  `0.0.0.0` s'il n'a qu'une interface), une réservation DHCP pour que son
  adresse ne change pas. **Ne redirigez jamais le port 8484 sur la box.** Sur
  un réseau partagé, préférez une option chiffrée.
- **Tailscale** (recommandé) : `tailscale up` sur le serveur et chaque
  appareil, puis `ANTHROPIC_BASE_URL=http://<nom-du-serveur>:8484`. Passez
  `"bind"` à l'adresse Tailscale (`100.x.y.z`) pour n'écouter que sur le
  tailnet. `tailscale serve --bg http://127.0.0.1:8484` ajoute du HTTPS
  (avec `"bind": "127.0.0.1"`). WireGuard, ZeroTier ou NetBird s'utilisent de
  la même manière.
- **Tunnel SSH** (rien à installer) : avec `"bind": "127.0.0.1"` sur le
  serveur,

  ```bash
  ssh -N -L 8484:127.0.0.1:8484 user@serveur
  export ANTHROPIC_BASE_URL=http://127.0.0.1:8484
  ```

  C'est la configuration la plus fermée ; `autossh` ou
  `ServerAliveInterval 30` maintiennent le tunnel.

Exposer le proxy sur Internet (Cloudflare Tunnel, reverse proxy public) ne
laisse que la clé d'appareil entre le monde et vos jetons Anthropic : placez
au minimum une authentification en amont (Cloudflare Access, par exemple).

## Panel et endpoints

Toutes les routes d'administration acceptent la clé d'un appareil enregistré,
en `?key=<clé>` ou en `Authorization: Bearer <clé>`.

| Route | Contenu |
|---|---|
| `GET /rotate/panel?key=<clé>` | panel HTML : appareils, jauges 5 h et 7 j par compte, alertes, consommation sur 24 h avec équivalent en dollars |
| `GET /rotate/status` | JSON : comptes, compte actif, utilisation, derniers changements de compte (`recent_events`) |
| `GET /rotate/stats` | JSON : agrégats sur 24 h par appareil, modèle et compte, alertes |
| `POST /v1/chat/completions` | endpoint compatible OpenAI, voir ci-dessous |
| toute autre route | relais transparent vers l'API Anthropic |

L'équivalent en dollars applique les tarifs publics de l'API
(`prices_per_mtok`), y compris lecture de cache à 0,1× et écriture à 1,25× :
c'est ce que l'usage coûterait sans abonnement.

### Endpoint compatible OpenAI

`POST /v1/chat/completions` accepte des requêtes au format OpenAI, en flux ou
non, et les sert avec les mêmes comptes :

```bash
curl http://<serveur>:8484/v1/chat/completions \
  -H "Authorization: Bearer <clé d'appareil>" -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-5","max_tokens":256,
       "messages":[{"role":"user","content":"bonjour"}]}'
```

- Conversations texte uniquement : une requête avec outils reçoit un 400.
- Quand tous les comptes sont épuisés (après l'éventuelle attente
  `hold_max_s`), la réponse est un **503** avec `Retry-After: <secondes avant
  la première réinitialisation>`, exploitable par le disjoncteur d'un routeur.
- Les réponses portent `x-rotate-account` et les en-têtes amont
  `anthropic-ratelimit-unified-*`.

Le relais transparent, lui, transmet tel quel le 429 d'Anthropic : Claude
Code le gère nativement.

## Configuration (`config.json`)

`claude-rotate init` crée le fichier depuis
[`config.example.json`](config.example.json). Tous les champs sont
optionnels ; les champs inconnus sont conservés par les commandes
d'administration.

| Champ | Défaut | Rôle |
|---|---|---|
| `bind` | `"127.0.0.1"` | adresse d'écoute ; `0.0.0.0` dans un conteneur |
| `port` | `8484` | port d'écoute |
| `upstream` | `"https://api.anthropic.com"` | URL de l'API amont ; utile pour les tests ou un relais intermédiaire |
| `accounts[]` | `[]` | `{name, token_file}` : un jeton `claude setup-token` par abonnement, sous `tokens/` |
| `accounts[].priority` | `100` | plus bas = préféré ; les comptes d'un palier supérieur ne servent que quand tous ceux du palier préféré sont épuisés |
| `accounts[].disabled` | `false` | `true` écarte le compte de la rotation tout en le gardant visible dans le panel |
| `devices{}` | `{}` | nom d'appareil vers clé d'appareil (la valeur de `CLAUDE_CODE_OAUTH_TOKEN`) |
| `switch_threshold` | `0.8` | utilisation de la fenêtre de 5 h qui déclenche la rotation, dans ]0, 1] |
| `switch_threshold_7d` | `0.98` | utilisation de la fenêtre hebdomadaire qui rend un compte inutilisable, dans ]0, 1] |
| `strategy` | `"consume-first"` | `consume-first` ou `least-used`, voir ci-dessous |
| `switch_cooldown_s` | `300` | délai minimal entre deux rotations volontaires ; une limite dure l'ignore |
| `switch_margin` | `0.05` | hystérésis : un candidat doit être meilleur d'au moins cette marge |
| `consume_first_margin_s` | `3600` | rotation proactive seulement si la réinitialisation hebdomadaire du candidat arrive au moins ce délai plus tôt |
| `hold_max_s` | `0` | quand tous les comptes sont épuisés, garder la requête ouverte jusqu'à ce délai en attendant une réinitialisation ; `0` renvoie l'erreur immédiatement |
| `prices_per_mtok` | tarifs publics | motif (sous-chaîne de l'id de modèle) vers `[entrée, sortie]` en $ par million de tokens |
| `alerts.device_tokens_per_hour` | `5000000` | alerte quand un appareil dépasse ce volume par heure |
| `alerts.expensive_model_patterns` | `["opus", "fable", "mythos"]` | modèles signalés comme coûteux |
| `alerts.util_warn` | `0.8` | alerte quand un compte approche ce taux d'utilisation |
| `public_url` | `""` | URL publique affichée par `add-device` et `onboard` |

Au démarrage, le serveur refuse une configuration dangereuse : clé d'exemple
encore présente (elle est publique), clé vide ou partagée entre deux
appareils, compte déclaré deux fois, seuil hors bornes.

## Politique de rotation

Un compte est *utilisable* tant que sa fenêtre de 5 h est sous
`switch_threshold` et sa fenêtre hebdomadaire sous `switch_threshold_7d` ; une
fenêtre dont la réinitialisation est passée compte pour 0 %. L'état survit aux
redémarrages dans `state.json`.

- **`consume-first`** (défaut) : utiliser d'abord le compte dont la fenêtre
  hebdomadaire se réinitialise le plus tôt. Le quota hebdomadaire est perdu
  s'il n'est pas consommé ; le compte le plus périssable passe donc en
  premier. Le proxy bascule aussi de façon proactive, sous le seuil, quand un
  autre compte se réinitialise au moins `consume_first_margin_s` plus tôt.
- **`least-used`** : le compte le moins utilisé sur la fenêtre de 5 h.
- **Paliers** : la rotation se fait dans le palier `priority` le plus bas qui
  a encore un compte utilisable. Quand un compte préféré redevient disponible,
  le trafic y revient après le délai de refroidissement
  (`priority_recovery`).
- **Refroidissement et hystérésis** empêchent l'alternance rapide autour du
  seuil ; un quota réellement refusé provoque toujours une rotation immédiate.
- **429 de débit et 429 de quota** : un 429 par minute (quota non épuisé) ne
  provoque pas de rotation, qui déplacerait la rafale sur le compte suivant et
  jetterait son cache de prompt. Le compte est temporisé pendant
  `retry-after` secondes puis la requête est rejouée.
- **Attente jusqu'à réinitialisation** (`hold_max_s` > 0) : quand tous les
  comptes sont épuisés, la requête reste ouverte jusqu'à la prochaine
  réinitialisation au lieu d'échouer. Le délai d'expiration du client doit le
  tolérer.

Chaque rotation apparaît dans `recent_events` avec sa raison :
`quota_exhausted`, `utilization>=<seuil>`, `consume_first`,
`priority_recovery` ou `account_disabled`.

## Lancer comme service

### Linux (systemd)

```bash
sudo useradd --system --home-dir /var/lib/claude-rotate --create-home claude-rotate
sudo install -m 0755 claude-rotate /usr/local/bin/
sudo -u claude-rotate claude-rotate --root /var/lib/claude-rotate init
sudo cp deploy/claude-rotate.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now claude-rotate
```

L'unité [`deploy/claude-rotate.service`](deploy/claude-rotate.service) est
durcie (système de fichiers en lecture seule hors du répertoire de données,
pas d'élévation de privilèges, `/tmp` privé). Adaptez `User` et le chemin de
`--root` si vous installez ailleurs.

### macOS (launchd)

[`deploy/com.example.claude-rotate.plist`](deploy/com.example.claude-rotate.plist)
définit un agent utilisateur ; ses commentaires indiquent l'installation.
Le répertoire de données doit se trouver sur le disque local : launchd
refuse l'accès aux volumes externes.

### Windows

Lancez `claude-rotate serve` depuis une tâche planifiée « au démarrage de
session », ou enveloppez-le avec un gestionnaire de services comme
[NSSM](https://nssm.cc/).

## Dépannage

- **401 avec `claude-rotate: unknown device key`** : c'est le proxy qui
  refuse l'appareil. La clé `CLAUDE_CODE_OAUTH_TOKEN` ne correspond à aucune
  entrée de `devices{}` ; vérifiez-la, ou régénérez-en une et redémarrez.
- **401 dont le corps vient d'Anthropic** : le jeton du compte a été refusé.
  Sur un compte qui fonctionnait, il a presque toujours expiré ou été
  révoqué : produisez-en un nouveau avec `claude setup-token`, puis
  `claude-rotate add-account <nom>` et redémarrez.
- **Une requête semble bloquée** : si tous les comptes sont épuisés et que
  `hold_max_s` est défini, c'est l'attente jusqu'à réinitialisation. Un
  événement `hold` apparaît dans `/rotate/status`.
- **Une modification de config n'a pas d'effet** : la configuration est lue
  au démarrage ; redémarrez le service.
- **Le panel est vide** : les tableaux se construisent depuis
  `logs/audit.jsonl` sur les dernières 24 h ; une installation neuve affiche
  des zéros jusqu'au premier trafic.
- **Surveillance** : `curl -fs "http://<serveur>:8484/rotate/status?key=<clé>"`
  échoue quand le proxy est arrêté.

## Sécurité

- `tokens/*.token` sont des identifiants au porteur valables un an sur vos
  comptes Anthropic. Ils ne quittent pas le serveur ; sous Unix, `tokens/` est
  en 0700 et chaque jeton, comme `config.json`, en 0600. Les écritures de
  configuration sont atomiques (fichier temporaire puis renommage).
- Les noms de compte et d'appareil sont limités à `[A-Za-z0-9._-]` : un nom
  de compte devient un nom de fichier et ne peut pas sortir de `tokens/`.
- Les clés d'appareil n'authentifient que l'accès au proxy ; une clé par
  machine permet d'en révoquer une seule (`remove-device`) sans toucher aux
  autres.
- Les jetons n'apparaissent jamais dans les journaux.
- `config.json`, `tokens/`, `state.json` et `logs/` sont exclus de git ; à
  sauvegarder : `config.json` et `tokens/`. `state.json` et `logs/` se
  reconstruisent.
- N'exposez pas le port 8484 sur Internet.

## Différences avec l'original Python

- **CLI intégrée** : `init`, `add-account`, `add-device`, `onboard`
  remplacent `setup.sh`, sans bash, openssl ni python, et fonctionnent sous
  Windows, Linux et macOS. `remove-device` est nouveau.
- **Formats compatibles** : `config.json`, `state.json` et
  `logs/audit.jsonl` gardent le même schéma ; les commandes d'administration
  conservent les champs inconnus et l'ordre des clés.
- **Un seul binaire** sans dépendance d'exécution (TLS en rustls), plus une
  image Docker.
- **Découpage SSE robuste** : les événements sont reconstitués même quand une
  ligne `data:` est coupée entre deux paquets réseau, et les flux sont relayés
  à l'identique.
- **Panel échappé** : toutes les valeurs dynamiques (noms d'appareil, modèles)
  sont échappées dans le HTML.
- **Champ `upstream`** : l'URL amont est configurable.

## Crédits et licence

Portage en Rust de [claude-rotate](https://github.com/doxaras/claude-rotate),
© 2026 John Doxaras, publié sous licence MIT. La conception (rotation
consume-first, distinction 429 de débit et de quota, attente jusqu'à
réinitialisation, panel) vient de ce projet.

Distribué sous licence [MIT](LICENSE). La note sur les conditions
d'utilisation en tête de ce document fait partie du contrat : ce logiciel sert
à faire tourner des comptes que *vous* possédez et payez, à vos risques.
