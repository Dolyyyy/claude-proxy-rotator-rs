# Accueil d'un membre de l'équipe

Comment une personne obtient un accès Claude Code via claude-rotate.

## Pour la personne qui demande un accès

Envoyez deux informations à l'administrateur :

1. **votre nom d'utilisateur**, court et en minuscules (par exemple `maria`) ;
2. **un nom d'appareil** pour la machine utilisée (par exemple `laptop`,
   `desktop`, `gpu-box`) : une demande par machine.

Seuls les lettres, chiffres, `.`, `_` et `-` sont acceptés.

Vous recevez en retour deux variables d'environnement, en syntaxe bash/zsh et
PowerShell. Ajoutez-les au profil de votre shell (`~/.zshrc`, `~/.bashrc` ou
`$PROFILE` sous Windows), ouvrez un nouveau terminal et lancez `claude`, sans
connexion Anthropic. Le lien vers votre tableau de bord d'usage accompagne la
réponse.

Règles :

- La clé est **propre à cet appareil**. Ne la partagez pas, ne la committez
  pas, ne la réutilisez pas sur une deuxième machine : demandez-en une autre.
- Si une machine est perdue ou si la clé a fuité, prévenez l'administrateur :
  la révocation est immédiate et ne concerne que cet appareil.

## Pour l'administrateur

Sur le serveur :

```bash
claude-rotate --root /var/lib/claude-rotate onboard maria laptop
```

(ou depuis le répertoire de données, sans `--root` ; avec l'unité systemd
fournie, lancez la commande sous l'utilisateur du service :
`sudo -u claude-rotate claude-rotate --root /var/lib/claude-rotate onboard maria laptop`).

La commande :

1. génère la clé de l'appareil `maria-laptop` (refusée si ce nom existe
   déjà) ;
2. sous Linux, redémarre le service systemd `claude-rotate` s'il est actif,
   directement ou via `sudo -n`, pour que la clé soit active tout de suite.
   Si le redémarrage n'est pas possible, elle l'indique ; ailleurs, redémarrez
   le serveur vous-même ;
3. affiche un bloc de bienvenue prêt à coller, avec l'URL exacte si
   `public_url` est renseigné dans `config.json`.

Transmettez ce bloc par un canal raisonnablement privé : la clé est un secret
au porteur.

**Révoquer un appareil :**

```bash
claude-rotate --root /var/lib/claude-rotate remove-device maria-laptop
sudo systemctl restart claude-rotate
```

Le panel liste toutes les clés (suffixe seulement) avec leur dernière
activité, ce qui permet de repérer les appareils inutilisés.

**Convention de nommage.** Le format `<utilisateur>-<appareil>` rend les
statistiques lisibles : les tableaux par appareil du panel deviennent des
tableaux par personne sans effort supplémentaire.

**Rappel.** Partager l'accès à des abonnements grand public avec des tiers
enfreint les conditions d'Anthropic : relisez la note sur les conditions
d'utilisation du [README](README.md) avant d'ouvrir des accès.
