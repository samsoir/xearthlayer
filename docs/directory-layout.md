# Where XEarthLayer Keeps Its Files

From v0.5.0, XEarthLayer follows the conventions of the system it runs on
instead of putting everything in `~/.xearthlayer`.

## The layout

`$XDG_CONFIG_HOME` and its siblings are honoured when set. The paths below are
the fallbacks.

| What | Linux | macOS |
|---|---|---|
| Configuration | `~/.config/xearthlayer/` | `~/Library/Application Support/XEarthLayer/` |
| Tile cache and indexes | `~/.cache/xearthlayer/` | `~/Library/Caches/XEarthLayer/` |
| Packages and patches | `~/.local/share/xearthlayer/` | `~/Library/Application Support/XEarthLayer/` |
| Logs and run state | `~/.local/state/xearthlayer/` | `~/Library/Logs/XEarthLayer/` |

On macOS, configuration and data are the same directory. That is Apple's
convention, not an oversight.

Every one of these is a default. `cache.directory`,
`packages.install_location`, `packages.temp_dir`, `patches.directory` and
`logging.file` all override them, and a large cache on a dedicated drive is a
perfectly sensible thing to configure. See
[configuration.md](configuration.md).

To see where your own installation resolves everything:

```bash
xearthlayer migrate status
xearthlayer config list
```

## Upgrading from an earlier version

The migration runs automatically the first time you start XEarthLayer after
upgrading. You do not need to do anything.

**Only `config.ini` is relocated, and it is copied rather than moved.** No
scenery, no cache and no packages are moved by us. Anything that sits at an old
location and holds data has its old path written into the migrated
configuration, so it keeps working exactly where it is.

That is a deliberate limit. Whether two directories are on the same filesystem
is not a question we can answer reliably: `rename` fails across btrfs
subvolumes, bind mounts and overlayfs layers, all inside what `df` reports as
one filesystem. A migration that sometimes moved half a terabyte and sometimes
did not, depending on the machine, would be worse than one that never moves
anything.

If some locations were kept where they are, XEarthLayer says so and tells you
how to review them:

```bash
xearthlayer migrate            # review and change locations
xearthlayer migrate --dry-run  # show what would change, change nothing
xearthlayer migrate status     # what has run, what is pending
```

Accepting the proposed layout can never break a working installation. You are
only asked about locations where there is a real choice, and keeping things
where they are is always the default.

### Rolling back

The old `~/.xearthlayer/config.ini` is kept, its paths rewritten to the new
locations. An older XEarthLayer still reads it and still finds everything, so
downgrading works. A comment at the top of that file records that it is no
longer the one in use, in case you edit it out of habit.

## Cleaning up the old directory

Nothing is deleted for you. After migrating, `~/.xearthlayer` still holds
whatever was there, plus a `MIGRATED.txt` recording what happened and where
things went.

**Safe to delete once the migration has run:**

| File | Why |
|---|---|
| `scenery_index.cache` | Stale. Rebuilt in the new location on the next scan |
| `ortho_union_index.cache` | Stale. Rebuilt automatically on the next start |
| `version_check.json` | Refetched within a day |
| `xearthlayer.log` | A previous run's log |
| `config.ini` | Only if you will not roll back to an earlier version |

**Check before deleting:**

| Directory | Why |
|---|---|
| `packages/` | Installed scenery. Reinstallable, but that is a large download |
| `patches/` | **Your own files.** Nothing can regenerate these |
| `cache/` | Tile cache from an older layout. Refills on demand, so it is safe to delete, though doing so costs a slow first flight |

Run `xearthlayer migrate status` first: if it reports the pre-0.5.0 directory as
absent, there is nothing to clean up.

To move rather than delete, for example to relocate installed scenery to the
new layout:

```bash
mkdir -p ~/.local/share/xearthlayer
mv ~/.xearthlayer/packages ~/.local/share/xearthlayer/packages
xearthlayer config set packages.install_location ~/.local/share/xearthlayer/packages
```

Set the configuration value in the same breath as the move. XEarthLayer pinned
the old path during migration precisely so that nothing broke, and that pin is
still in your configuration until you change it.

Once the directory holds nothing you want:

```bash
rm -rf ~/.xearthlayer
```

## Why this changed

Reported in [#193](https://github.com/samsoir/xearthlayer/issues/193): a home
directory accumulates one dotted directory per application, and the platform
conventions exist to stop that. Separating the roles also means a cache cleaner
can reclaim `~/.cache` without touching configuration, and a backup of
`~/.config` captures settings without sweeping up hundreds of gigabytes of
regenerable tiles.
