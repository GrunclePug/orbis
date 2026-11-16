# orbis

A Kerbal Space Program mod manager with Prism-style instances, built on CKAN metadata.

![orbis](docs/screenshot.png)

## Overview

orbis is a Linux rewrite of [CKAN](https://github.com/KSP-CKAN/CKAN) for KSP 1. Every instance has its own mods, saves and settings, like Prism Launcher instances for Minecraft, and links the stock game from your Steam install without ever changing it, so launching KSP from Steam stays vanilla. Mods come from the CKAN index with full dependency resolution, and mods CKAN doesn't carry (Patreon builds, betas, personal patches) live in a manual mod library that any instance can use. It ships as a single binary with a GUI and a scriptable CLI, and it launches the Windows build of KSP through the Proton that Steam uses.

## Features

* **Instances**: as many as you like, each holding only its mods, saves and settings (about 15 MB) and linking the stock game from your Steam install, which is never written to.
* **CKAN mods**: browse, search, install, upgrade and remove the whole CKAN catalogue, with dependencies, conflicts, recommendations and provider choices handled the way CKAN does.
* **Dependencies out of the way**: the Installed list shows the mods you chose; dependencies stay hidden behind a checkbox and say which mods need them.
* **Starred mods**: star the mods you always use, then show only starred mods in any list and queue them all at once in a new instance.
* **Modpacks**: export an instance's mods (optionally with craft, saves and manual mod files) and import them as a new instance; CKAN's `.ckan` modpacks work both ways.
* **Remembers where you were**: the selected instance, filters and unapplied changes survive a restart.
* **Manual mods**: add a `.zip` or folder once, then install it into any instance. A manual mod can replace CKAN mods (for example, a Patreon Volumetric Clouds build standing in for EVE Redux and Scatterer), so mods that depend on them keep working.
* **Proton launch**: start any instance with the Proton build Steam has mapped to KSP, inside the Steam Linux Runtime when Proton asks for it.
* **Per-instance settings**: compatible KSP versions, testing releases, launch arguments, environment variables, and a shared or separate Wine prefix.
* **Reliable downloads**: parallel downloads that drop stalled connections and resume where they left off, falling back to CKAN's archive.org mirror for freely licensed mods.
* **Safe file handling**: every installed file is tracked per mod, conflicts are refused, and a failed install rolls back.
* **GUI and CLI**: an egui interface, plus a CLI covering every operation.
* **Headless rendering**: the GUI renders offscreen to PNG with a text state dump, so it can be checked without a display.
* **Pure Rust**: a single binary with no C dependencies; downloads go through the system `curl`.

## Installation

### Requirements

* Rust 1.100 or newer and Make
* `curl` at runtime
* A Vulkan driver (any GPU driver, or lavapipe for software rendering)
* Steam with KSP installed, set to run under Proton (Proton-GE or Valve Proton)

### Build

```sh
git clone https://github.com/grunclepug/orbis
cd orbis
make build
sudo make install    # optional, installs to /usr/local/bin
```

## Usage

Run `orbis` with no arguments to open the GUI:

1. Press **Update index** to download the CKAN catalogue.
2. Press **+ New** to create an instance from your Steam install.
3. Tick mods in the **Compatible** list, press **Apply changes**, and confirm.
4. Press **Launch**.

The same steps from the CLI:

```sh
orbis update
orbis instance create "Stock Career"      # linked to the Steam install
orbis search scatterer -i "Stock Career"
orbis install -i "Stock Career" Scatterer EnvironmentalVisualEnhancements
orbis launch "Stock Career"
```

When only one instance exists, `-i` can be left out.

### CLI commands

| Command | Action |
|---------|--------|
| `orbis gui` | Open the GUI (the default with no command) |
| `orbis update` | Download the latest CKAN index |
| `orbis search <query> [-i <instance>]` | Search mods, optionally only those compatible with an instance |
| `orbis show <identifier>` | Show a mod's details, relationships and releases |
| `orbis list [-i <instance>]` | List installed mods and detected DLC |
| `orbis install [-i <instance>] <mod>[=<version>]...` | Install mods and their dependencies |
| `orbis remove [-i <instance>] <mod>...` | Remove mods and anything that depends on them |
| `orbis upgrade [-i <instance>] [<mod>...]` | Upgrade the named mods, or every upgradable one |
| `orbis instance list\|create\|clone\|sync\|rename\|delete\|info\|compat` | Manage instances |
| `orbis manual list\|add\|show\|install\|delete` | Manage the manual mod library |
| `orbis launch [<instance>] [--dry-run]` | Start an instance, or print the command |
| `orbis steam` | Show the detected Steam, KSP and Proton paths |
| `orbis star\|unstar <mod>...` | Star or unstar mods (shared by all instances) |
| `orbis starred` | List starred mods |
| `orbis pack export [-i <instance>] <file>` | Export a modpack (`--craft`, `--saves <names\|all>`, `--manual-files`, `--ckan`) |
| `orbis pack import <file>` | Create an instance from an `.orbispack` or CKAN `.ckan` modpack (`--name`, `--from <dir>`, `--latest`) |

`install`, `remove` and `upgrade` show the plan and ask before applying it. Pass `-y` to skip the question, `--dry-run` to only show the plan, and `--choose '<relationship>=<identifier>'` to answer a provider choice.

### Configuration

Settings live in `~/.config/orbis/config.toml` and can also be edited on the GUI's Settings tab.

| Variable | Flag | Default | Description |
|----------|------|---------|-------------|
| `steam_root` | | detected | Steam's root directory |
| `proton` | | the Proton Steam maps to KSP | Directory holding the `proton` script |
| `use_steam_runtime` | | `true` | Run Proton inside the Steam Linux Runtime when Proton asks for it |
| `repositories` | | CKAN-meta `master.tar.gz` | CKAN metadata archives, each a `{ name, url }` table |
| `download_workers` | | `4` | Parallel mod downloads (1 to 16) |
| | `--root <dir>` | XDG directories | Keep config, data and cache under one directory |
| `RUST_LOG` | | `warn` | Log level, written to stderr |

Per-instance settings (compatible versions, testing releases, launch arguments, environment, prefix) live in each instance's `instance.toml` and on the Settings tab.

## Instances

An instance is a directory under `~/.local/share/orbis/instances/<id>/`:

```
instance.toml   name, linked install, game version, compatible versions, launch settings
registry.json   installed mods and the files each one owns
game/           the KSP directory the game runs from
prefix/         a separate Wine prefix, when the instance uses one
```

By default an instance links to a game install (the Steam one, or `--from <dir>`). Its `game/` directory holds real copies only of what belongs to the instance:

* `GameData`, with the stock `Squad` and `SquadExpansion` folders linked in and your mods installed beside them
* `saves` (starting with the stock scenarios and training; add `--with-saves` to copy your existing saves)
* `Ships`, `Missions`, `UserLoadingScreens`, `settings.cfg` and `Physics.cfg`

Everything else (`KSP_x64.exe`, `KSP_x64_Data`, the DLLs and so on) is a symlink into the install. orbis never writes to the install: the installer refuses any file that would land behind a link, and deleting an instance removes only the links. Launching KSP from Steam therefore stays vanilla, and Steam Cloud never sees an instance's saves.

Before every launch, orbis re-checks the links (`orbis instance sync` does the same by hand): it adds links for files a Steam update introduced, drops links to removed ones, and records the install's current game version. If you'd rather an instance keep its game version when Steam updates, create it with `--copy` (or tick **Full copy** in the GUI) for a complete, independent copy. On btrfs or XFS that copy is reflinked, so it is instant and takes no extra space.

Instances share Steam's KSP Wine prefix by default. That's safe because KSP keeps its saves and settings in the game directory. Switch an instance to a separate prefix in its settings if you want full isolation; Proton builds it on first launch.

## Manual mods

The manual library lives in `~/.local/share/orbis/manual/`. Add a mod in the GUI (**Manual mods > + Add mod**, or drop a file on the window) or with the CLI:

```sh
orbis manual add ~/Downloads/VolumetricClouds.zip --name "Volumetric Clouds" --version 5.0 \
    --provides EnvironmentalVisualEnhancements,Scatterer
orbis manual install volumetric-clouds -i "Stock Career"
```

orbis works out where the files go: if the archive has a `GameData` folder anywhere, its contents are installed; otherwise each top-level folder goes into `GameData`. Use `--find <folder>` to install a single folder instead. Only `.zip` archives and plain folders are supported; unpack other formats first.

`--provides` lists the CKAN identifiers the mod replaces. Installing it removes the CKAN copies, and CKAN mods that depend on those identifiers are satisfied by the manual mod. `--depends` and `--conflicts` take CKAN identifiers too.

## Modpacks

An `.orbispack` is a zip with a `pack.toml` (the mod list at exact versions, which mods are dependencies, compatible game versions and launch settings) plus whatever you chose to include:

```sh
orbis pack export -i "Stock Career" career.orbispack --craft --saves "My Career"
orbis pack import career.orbispack --name "Career (friend)"
```

Importing creates a new linked instance, installs the pack's exact versions (or the newest compatible ones with `--latest`; a version that has left CKAN falls back to the newest with a warning), copies in the saves and craft, and keeps dependencies marked as dependencies. A pack with saves and craft makes a custom scenario: share it and the other side starts from the same point.

Manual mod files are left out unless you pass `--manual-files`, since mods like Patreon builds usually aren't yours to share. Without them, the pack still lists the manual mod; importing reuses your library copy when you have the same version, and warns otherwise.

`--ckan` writes CKAN's modpack format instead (mods only), and `.ckan` modpacks from CKAN import like any other pack. In the GUI, use **Export** under the instance list and **Import** beside **+ New**, or drop a pack file on the window.

## Launching

orbis runs `KSP_x64.exe` from the instance's `game/` directory with the Proton build Steam has mapped to KSP, passing the same `STEAM_COMPAT_*` environment Steam does (plus `STEAM_COMPAT_MOUNTS`, so the runtime container can see the linked install). When that Proton requires the Steam Linux Runtime, orbis starts it through the runtime's entry point just as Steam does. KSP 1 has no DRM, so Steam doesn't need to start the game, though the Steam overlay and playtime tracking don't apply. Proton's output goes to `orbis-launch.log` in the instance directory. `orbis launch --dry-run` prints the exact command.

## Development

```sh
make run ARGS="gui"   # run a debug build
make test             # unit and integration tests
make lint             # clippy with warnings as errors
make fmt              # rustfmt
make build            # release build
make gui-shot         # headless GUI render to target/gui.png and target/gui.txt
make clean            # remove build output
```

## Contributing

Fork the repository and open a pull request.

## License

MIT. See [LICENSE](LICENSE). orbis reads metadata from CKAN, which is MIT licensed by the CKAN authors.

## Author

Chad Humphries | [Website](https://grunclepug.com/) | [GitHub](https://github.com/GrunclePug) | [More projects](https://github.com/GrunclePug?tab=repositories)
