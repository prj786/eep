# The eep card — for Files, Komble and the website

A card is one small TOML file, `<Name>.eep`, MIME type `application/x-eep`.
**The card in `~/Applications` is the app.** Everything else follows:

| The user… | What happens |
|---|---|
| drops a card into `~/Applications` (from Downloads, the website, Komble) | `eep watch` installs it and says how far it is (see "While an app installs") |
| double-clicks a card in `~/Applications` | the app starts (`eep open <file>`) |
| double-clicks a card anywhere else | the installer shows the app's icon and name over an **Install** button (`eep open --ask <file>` hands the card to ewe Files); Install moves the card into `~/Applications` (`eep add <file>`) |
| moves a card to Trash / deletes it | the app leaves the launcher at once |
| restores it from the Trash | it comes back with no download (an app in the Trash is kept whole) |
| empties the Trash (or deleted it outright) | its disk is freed by itself (`eep gc`, run every few hours) |
| renames it | nothing: the `id` inside counts |

**Which version gets installed.** A card whose app is not on this computer
yet is installed as eep's index has the app *today*: the current version,
with update patches, and never the store paths a downloaded file names. The
card is rewritten to say what was installed. Only an app the index does not
have (another channel, a retired app) is fetched exactly as its card names
it. A card whose store path is here already (back from the Trash) is used
as it is, with no download.

## While an app installs (eep 0.3)

A card in `~/Applications` is an app, but one that just arrived is not
there yet. Two things say so, and a file manager needs only these:

**Is it ready?** eep keeps a link to the store path of every app it has
fetched: `$XDG_STATE_HOME/eep/roots/<id>` (default `~/.local/state/eep/roots/`).
An eep app can start when that link points at the card's `out`. (A found
app, `source = "system"`, is always ready.) One `readlink`; the card does
not change when the app becomes ready, so ask whenever you list the folder.

**How far is it?** `$XDG_RUNTIME_DIR/eep/activity.json` (`~/.cache/eep/`
in a session without a runtime directory) lists what eep is fetching right
now. It is written a few times a second while something downloads, by
rename (never half a file), and removed when nothing is in flight:

```json
{ "eep_activity": 0, "updated": 1763895000, "apps": [
  { "id": "blender", "name": "Blender",
    "card": "/home/you/Applications/Blender.eep",
    "what": "install", "state": "downloading",
    "done": 1048576, "total": 8388608, "download": 2097152, "pid": 1234 } ] }
```

| Field | |
|---|---|
| `what` | `install` or `update` (an app that is updating still starts) |
| `state` | `waiting` (eep is busy with another app) → `downloading` → `finishing` (launcher entry, card); or `failed` with `error` in plain words |
| `done`, `total` | bytes unpacked so far and in all; 0 = not known yet. Apps fetched together share one count. Never goes backwards |
| `download` | bytes the whole fetch takes off the network (for "120 of 264 MB": `download × done ÷ total`) |
| `card` | the app's card (absent for an install from a store, until it is done) |
| `pid` | the eep that says so. **Ignore an entry whose pid is gone** (eep was killed); eep's next write drops it |

A card with no entry that is not ready is waiting for `eep watch` to start
(half a second), or for another eep to finish. `failed` entries stay while
`eep watch` runs: it tries again by itself (after 20 s, then less and less
often, and at once when anything in `~/Applications` changes); `eep add
<card>` is "try again now". Taking the card out of `~/Applications` while
its app downloads stops the download.

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
| open a card | `eep open <file>` (becomes the app; installs it first if needed). `--ask`: when it is not installed, the installer (ewe Files, or `$EEP_INSTALLER`) shows it instead |
| what a card file is, before installing | `eep card <file>` → `installed` (the card holding this app in Applications), `ready`, `inside`, `download` + `pieces` (what an install would fetch now) |
| install from a card file | `eep add <file>`: moves it into `~/Applications`, installs, answers when done (`{"ok":true,"id","card"}`); the app is not started |
| is eep set up here | `eep doctor` |

## Files

`~/Applications` (`$EEP_APPS_DIR` overrides; it was `~/Apps` before
2026-11-22 and moves by itself). Downloadable cards for a release:
`dist/<release>/cards/<Name>.eep`; their list: `dist/<release>/catalog.json`
(`eep-build cards --release <R>`).
