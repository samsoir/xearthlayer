# SimHeaven X-World Pro

[SimHeaven X-World Pro](https://simheaven.com) is a commercial set of overlay packages for X-Plane 12: bridges, buildings, forests, roads and more, sold per continent or as a world bundle. XEarthLayer also ships overlays, so on a continent where both are active they draw over each other. This guide describes how XEarthLayer steps aside for X-World Pro.

## What XEarthLayer does with X-World Pro

- Detects X-World Pro by reading `scenery_packs.ini` in your Custom Scenery folder. Folders you copied there and folders you symlinked there are treated the same.
- Works out which continents have at least one enabled, existing X-World Pro layer.
- Leaves its own overlays out of the consolidated `yzXEL_overlay/` folder for those continents. When the whole planet is active, it leaves out every overlay.
- Checks the order of `scenery_packs.ini` and the state of your X-World Pro installation on every start, and tells you what it finds.

XEarthLayer never edits `scenery_packs.ini`, X-World Pro's files, or anything inside X-Plane. It only advises. Overlay packages stay installed, so turning the feature off restores them with no download.

## Turning it on

Nothing changes until `packages.xworld_pro` is `true`. It defaults to `false`.

- `xearthlayer setup` has a step, "Third-party Scenery" (Step 6), that looks for X-World Pro and asks "Give X-World Pro priority over XEarthLayer overlays?". Answer yes and it saves `true` and prints three messages: how to install X-World Pro for use with ortho scenery, a note on video memory and frame rate, and what XEarthLayer does on each start. Answer no and it saves `false` with a short note that XEarthLayer overlays may then draw over X-World Pro, followed by the same video memory note, which applies either way. If no X-World Pro is found it says so and saves `false`.
- Or set it directly:

  ```bash
  xearthlayer config set packages.xworld_pro true
  ```

With the setting off and X-World Pro installed, `xearthlayer run` prints a one-line hint suggesting one of the two ways above, and otherwise behaves as before.

`xearthlayer packages install`, `update` and `remove` follow the same rule: when they rebuild the consolidated overlay folder they leave out the continents X-World Pro covers, and say which regions were left out, so a package command no longer puts them back until the next `run`.

`packages.disable_overlays = true` still turns every XEarthLayer overlay off, whatever `xworld_pro` says. The two settings combine: `disable_overlays` is all continents, `xworld_pro` is only the ones X-World Pro covers. Use `xworld_pro` when you want XEarthLayer overlays to remain on continents that X-World Pro does not cover.

## Region codes and continents

A package's region code has the form `<CONTINENT>-<REGION>-<SUBREGIONS>`. The first segment, ignoring case, decides which X-World Pro continent covers it:

| Region code starts with | X-World Pro continent |
|---|---|
| `NA`, `SA` | America |
| `EU` | Europe |
| `AF` | Africa |
| `AS` | Asia |
| `OC` | Australia-Oceania |
| anything else | none |

Packages with no continent keep their overlays, unless the whole planet is active, which turns off every overlay. Folders named Antarctica count as Australia-Oceania, as in the X-World Pro manual. A continent is active when at least one of its layers is enabled in `scenery_packs.ini` and its folder exists (or its symlink resolves).

## The checks on every start

With `packages.xworld_pro = true`, `xearthlayer run` runs three preflight checks after it has found your Custom Scenery folder.

**`xworld-pro-detect`** records what it found, for example which continents are active and where XEarthLayer overlays are off, in the log at INFO level; the dashboard's Status pane shows the active continents too. It prints a warning only when something needs your attention: the setting is on but nothing is found, no continent is enabled, or `scenery_packs.ini` cannot be read. In each of those cases it changes nothing.

**`xworld-pro-order`** requires that every enabled XEarthLayer entry (`zzXEL_ortho`, and `yzXEL_overlay` when present) comes after every enabled X-World Pro layer in `scenery_packs.ini`. The library's position is not checked. If the order is wrong, it shows the file, the topmost enabled XEarthLayer entry and the lowest enabled X-World Pro layer with their line numbers, and how to fix it, then waits for a key:

- `s` (or Escape): XEarthLayer exits. Move XEarthLayer's entries below every X-World Pro line as the "Scenery_packs.ini" section of the X-World Pro manual describes, then start again.
- `c`: XEarthLayer starts, and the problem stays in the log and as a startup warning on the dashboard. The answer is remembered: XEarthLayer does not ask again until `scenery_packs.ini` changes, and any change to it, by you or by X-Plane adding new scenery, asks again.

It asks only when it can be both seen and answered, which means both standard input and standard error are a terminal. Otherwise (a script, a service, output redirected to a file) it continues with the warning and remembers nothing.

**`xworld-pro-setup`** only warns. It reports:

- a `library.txt` that differs from `library - ortho.txt`, so the library is not the ortho version;
- a `_O4XP-Mesh` / `_XP12-Mesh` pair, on an active continent, where the `_O4XP-Mesh` entry is not enabled or the `_XP12-Mesh` entry is not disabled. This works for every layer and continent by the naming convention, not only Europe's bridges;
- missing install links: the library's `mod/<season>/*.for` files expected under `Resources/default scenery/1200 forests/<season>/`, and `mod/Boats_1.dds` expected under `Resources/default scenery/sim objects/ships/`;
- broken entries: an enabled line whose folder is missing or whose symlink dangles.

## Installing X-World Pro for ortho scenery

Follow the X-World Pro manual. The points that matter alongside XEarthLayer:

1. In `scenery_packs.ini`, the manual's order: airports, regional scenery, global airports, X-World Pro packages, libraries, photo overlays, orthos last. XEarthLayer's entries (`yzXEL_overlay`, `zzXEL_ortho`) must come after every X-World Pro layer.
2. Use the ortho library: replace `library.txt` in the X-World Pro library folder with the contents of `library - ortho.txt`. It removes the farmland, meadow and grass polygons that would otherwise cover the imagery.
3. For any layer that ships two mesh versions, enable the `_O4XP-Mesh` one and disable its `_XP12-Mesh` twin. XEarthLayer packages use Ortho4XP meshes. In X-World Pro 1.1 only Europe's bridges have the pair.
4. After an X-Plane update, run the library's install script (`install_XWP_Linux_Mac.sh`) again in the X-World Pro library folder inside Custom Scenery. It links forest and boat files into X-Plane's `Resources`, and updates can remove those links.

## Performance

XEarthLayer and X-World Pro both use a lot of video memory. On graphics cards with less than 24 GB, lower X-Plane's rendering settings if your frame rate drops. X-World Pro with all layers costs about 5 to 10 fps on its own, even without ortho scenery.
