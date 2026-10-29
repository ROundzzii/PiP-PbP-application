# PiP-PbP application

Le **Picture-in-Picture** / **Picture-by-Picture** des écrans, mais entre deux PC :
l'écran d'un ordinateur s'affiche dans une fenêtre flottante sur l'autre, avec la
latence la plus faible possible. Écrit en Rust, pour Windows 10/11.

- Fenêtre sans bordure, toujours au premier plan, déplaçable et redimensionnable.
- Quand la souris passe sur la fenêtre, elle devient semi-transparente (opacité
  réglable) et les clics la traversent : on voit et on utilise ce qu'il y a derrière.
- Modes PiP (coin de l'écran) et PbP (moitié gauche / droite) en une touche.

## Comment ça marche

```
PC source                                        PC viewer
DXGI Desktop Duplication ─► BGRA→I420 ─► H.264 ─TCP─► H.264 ─► textures Y/U/V ─► shader YUV→RGB
  (capture GPU, ~0 ms)     (multithread)  (OpenH264)   (décodage)    (wgpu / DX12, présentation Mailbox)
```

Choix faits pour la latence :

- **Capture DXGI Desktop Duplication** : l'API la plus directe de Windows, réveillée
  uniquement quand l'écran ou la souris change.
- **H.264 sans B-frames, sans tampon** (OpenH264 en mode temps réel) : chaque image
  est envoyée dès qu'elle est encodée.
- **Aucune file d'attente** : si le réseau ne suit pas, l'émetteur saute des images au
  lieu d'accumuler du retard ; le viewer affiche toujours la dernière image décodée.
- **TCP_NODELAY**, conversion YUV→RGB et mise à l'échelle sur le GPU, présentation
  `Mailbox` (pas d'attente du vsync).

Mesuré sur un PC de test (RTX 3060, encodage logiciel) : 1080p ≈ 2 ms de conversion
+ 7 ms d'encodage, 1440p ≈ 3 + 11 ms, décodage ≈ 7 ms.

## Installation

Prérequis sur la machine qui compile :

1. [Rust](https://rustup.rs) (toolchain MSVC).
2. **NASM**, indispensable pour un encodeur rapide (sans lui OpenH264 est 5 à 30 fois
   plus lent ; le build affiche un avertissement) :

```bash
winget install NASM.NASM
```

Puis, avec NASM dans le `PATH` (par défaut `%LOCALAPPDATA%\bin\NASM`) :

```bash
cargo build --release
```

L'exécutable unique `target/release/pip-pbp.exe` sert aux deux PC.

## Utilisation

L'application tourne sur **les deux PC**, mais il n'y a rien à installer : on copie
le même `pip-pbp.exe` (autonome, sans DLL) sur chacun. Pas de compte ni de code à
partager comme avec AnyDesk : les deux PC doivent simplement être sur le même réseau
local, et le viewer trouve la source tout seul (broadcast UDP).

### Interface graphique

Double-cliquer sur `pip-pbp.exe` (ou le lancer sans argument) ouvre l'interface :

- **Connexions** : rechercher les sources du réseau, enregistrer des connexions
  (nom, IP, port), les modifier / supprimer, connexion rapide par IP.
- **Partager cet écran** : démarrer / arrêter le partage, choix de l'écran, fps,
  débit, résolution, curseur ; affiche les adresses IP de ce PC.
- **Réglages d'affichage** : opacité normale et au survol, clic à travers, premier
  plan, ratio, disposition PiP / PbP au démarrage, taille.
- **Raccourcis** : tous les raccourcis de la fenêtre PiP sont modifiables (cliquer
  puis appuyer sur la nouvelle combinaison). Les touches sont reconnues par leur
  position sur le clavier (AZERTY comme QWERTY).
- **Sessions** : fenêtres PiP et partage en cours, avec leur journal et un bouton Arrêter.

Les réglages et connexions sont enregistrés automatiquement dans
`%APPDATA%\pip-pbp\pip-pbp.toml` (ou dans un `pip-pbp.toml` placé à côté de l'exe,
pour un usage portable).

### Ligne de commande

Sur le PC dont on veut voir l'écran :

```bash
pip-pbp send
```

Sur le PC qui affiche :

```bash
pip-pbp view
```

Ou avec l'IP de la source, si la détection automatique est bloquée (réseaux séparés, VPN…) :

```bash
pip-pbp view 192.168.1.10
```

La première fois, Windows demande d'autoriser `pip-pbp.exe` dans le pare-feu **du PC
source** : cocher « Réseaux privés » et « Réseaux publics ». Le viewer se reconnecte
tout seul si la source redémarre.

Le dossier `packaging/` contient un `LISEZMOI.txt` à joindre à l'exe pour
le distribuer.

Autres commandes :

```bash
pip-pbp monitors
```

```bash
pip-pbp bench --monitor 1
```

`monitors` liste les écrans capturables, `bench` mesure le temps d'encodage du PC.

### Souris et clavier dans la fenêtre

**Alt est la touche d'interaction** : curseur sur la fenêtre PiP et Alt maintenu,
la fenêtre devient opaque et cliquable, les raccourcis Alt+touche fonctionnent et
**Alt + clic droit** ouvre le menu de toutes les options (il se ferme au
relâchement d'Alt). Sans Alt, rien n'est capté : les clics et les touches vont à la
fenêtre derrière. Seul le raccourci de contrôle (Ctrl+Alt+K) marche partout.

Raccourcis par défaut, modifiables dans l'onglet *Raccourcis*. Les actions dont
l'effet ne se voit pas tout de suite affichent une petite notification.

| Action | Effet |
|---|---|
| Survol | Fenêtre transparente (`hover_opacity`) et clics traversants |
| **Alt** maintenu | Fenêtre opaque et interactive (déplacer / redimensionner) |
| Alt + glisser | Déplacer la fenêtre |
| Alt + glisser un bord / coin | Redimensionner (proportionnel sauf en étirement libre) |
| Alt + molette | Agrandir / réduire en gardant le ratio |
| Alt + double-clic, **Alt+F11** | Plein écran / fenêtré |
| **Alt+1** / **Alt+2** / **Alt+3** | PiP coin bas-droit / PbP gauche / PbP droite |
| **Alt+↑** / **Alt+↓** | Opacité au survol ±5 % |
| **Alt+Page↑** / **Alt+Page↓** | Opacité normale ±5 % |
| **Alt+C** | Clic à travers on/off |
| **Alt+T** | Toujours au premier plan on/off |
| **Alt+A** | Étirement libre on/off |
| **Alt+L** | Compteur de latence (capture → affichage) on/off |
| **Alt + clic droit** | Menu des options |
| **Échap** | Quitter le plein écran |
| **Ctrl+Alt+K** | Contrôle de l'autre PC (KVM) : prendre / rendre la main |
| **Alt+Q** | Fermer la fenêtre PiP |

## Contrôle à distance (KVM)

La fenêtre PiP peut aussi piloter la souris et le clavier de l'autre PC.

1. Sur le PC source, onglet *Partager cet écran* : cocher **« Autoriser l'autre PC à
   piloter la souris et le clavier »** (désactivé par défaut). Un **code à 6 chiffres**
   s'affiche, puis démarrer le partage.
2. Sur le PC viewer, onglet *Connexions* : saisir ce code dans la connexion
   (bouton *Modifier*), puis l'afficher.
3. Dans la fenêtre PiP, **Ctrl+Alt+K** prend la main : liseré orange, la souris reste
   dans la fenêtre et pilote l'autre PC. **Ctrl+Alt+K** à nouveau rend la main.

Sécurité : le code n'est jamais envoyé en clair (preuve HMAC-SHA256 sur un nombre
aléatoire différent à chaque connexion), 5 essais faux coupent la connexion, et les
entrées sont ignorées tant que le code n'est pas validé. À la sortie du mode contrôle
(ou si la fenêtre perd le focus), toutes les touches et boutons enfoncés sont relâchés
sur l'autre PC. Le flux n'est pas chiffré : à utiliser sur un réseau de confiance ou
à travers un VPN.

Limites : les écrans protégés de Windows (demandes UAC, écran de verrouillage,
Ctrl+Alt+Suppr) ne peuvent pas être pilotés, et Alt+Tab / la touche Windows restent
captés par le PC local.

## Configuration

Copier [pip-pbp.example.toml](pip-pbp.example.toml) en `pip-pbp.toml` à côté de
l'exécutable. Les principales options en ligne de commande :

```bash
pip-pbp view 192.168.1.10 --hover-opacity 0.2 --layout pbp-right
```

```bash
pip-pbp send --monitor 1 --fps 60 --bitrate 30 --downscale 1
```

## Conseils pour la latence

- Réseau **filaire** (Ethernet) de préférence ; le Wi-Fi ajoute de la gigue.
- Écran source 4K : la résolution est réduite à 1080p par défaut (`downscale = 0`),
  `--downscale 1` pour du natif si le CPU suit (voir `pip-pbp bench`).
- `sharp_text = true` donne un texte plus net mais un encodage ~3× plus lent.

## Limites actuelles / pistes

- Encodage logiciel (OpenH264). Un encodeur matériel (NVENC / AMF / QuickSync via
  Media Foundation) diviserait encore le temps d'encodage, surtout en 4K.
- Écrans HDR non gérés (la capture attend du BGRA 8 bits).
- Pas de chiffrement : à utiliser sur un réseau local de confiance.
- Pas de son ni de contrôle à distance : c'est un affichage, comme le PiP d'un écran.
