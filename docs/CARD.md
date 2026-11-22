# The eep card — for Files, Komble and the website

A card is one small TOML file, `<Name>.eep`, MIME type `application/x-eep`.
**The card in `~/Applications` is the app.** Everything else follows:

| The user… | What happens |
|---|---|
| drops a card into `~/Applications` (from Downloads, the website, Komble) | `eep watch` installs it (fetches what it names, adds the launcher entry) |
| double-clicks a card anywhere | `eep open <file>`: copies it into `~/Applications` if needed, installs, starts the app |
| moves a card to Trash / deletes it | the app leaves the launcher at once |
| restores it from the Trash | it comes back with no download (an app in the Trash is kept whole) |
| empties the Trash (or deleted it outright) | its disk is freed by itself (`eep gc`, run every few hours) |
| renames it | nothing: the `id` inside counts |

## Apps that were on the computer already (eep 0.2)

Every app in the launcher gets a card, not only eep's: pacman packages,
AppImages, Steam games, the user's own launcher entries. Such a card says
`source = "system"`, its id starts with `sys-`, and it names the app's
launcher entry instead of store paths:

```toml
eep = 0
id = "sys-gimp"              # sys- + the desktop file id
name = "GIMP"
kind = "gui"
source = "system"
from = "pacman"              # pacman | appimage | steam | flatpak | local | system
entry = "gimp.desktop"       # the launcher entry it stands for
package = "gimp"             # pacman's package, when known
version = "3.2.8-1"          # pacman's version, when known
icon_type = "image/svg+xml"
icon_data = "…"
```

| The user… | What happens to a found app |
|---|---|
| moves its card to Trash / deletes it | it leaves the launcher at once (eep writes a `NoDisplay` copy of its entry marked `X-Eep-Hides`; file types still open with it) |
| restores the card | it is back in the launcher (the copy goes; a user's own entry comes back byte for byte) |
| uninstalls it (pacman, Komble) | its card goes by itself |
| double-clicks the card | the app starts (GIO launches its entry) |

Nothing is downloaded or uninstalled for a found app. Stores that list
their own pacman/AppImage apps should skip `source = "system"` cards
(`eep list` leaves them out; `eep list --all` has them).

## Fields a file manager or store needs

```toml
eep = 0                      # format version
id = "firefox"               # stable app id
name = "Firefox"             # show this, not the file name (no ".eep")
kind = "gui"                 # gui | cli | tool
version = "160.0.1"
icon_type = "image/png"      # or "image/svg+xml"; may be absent
icon_data = "iVBORw0KGgo…"   # base64 of the icon file (≤ 96 KB decoded); may be absent
```

Everything else in the card is eep's (store paths, release, previous
version). Read it with any TOML parser; ignore unknown fields; never write
a card yourself — let eep do it (`eep install <id>` writes one).

## Commands (JSON on stdout, exit 0; `{"ok":false,"error":…}` on failure)

| Need | Command |
|---|---|
| what can be installed (with `icon_url`, `installed`, `update`) | `eep available` |
| install / uninstall | `eep install <id>` · `eep remove <id>` (a `sys-` id: show / hide) |
| updates (badge: `--count`) | `eep updates` · `eep updates --count` · `eep update [ids…]` |
| installed apps | `eep list` (eep's) · `eep list --all` (every card) |
| open a card | `eep open <file>` (becomes the app) |
| is eep set up here | `eep doctor` |

## Files

`~/Applications` (`$EEP_APPS_DIR` overrides; it was `~/Apps` before
2026-11-22 and moves by itself). Downloadable cards for a release:
`dist/<release>/cards/<Name>.eep`; their list: `dist/<release>/catalog.json`
(`eep-build cards --release <R>`).
