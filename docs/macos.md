# XEarthLayer on macOS

XEarthLayer supports macOS (Apple Silicon tested) using [macFUSE](https://macfuse.io) to provide the FUSE virtual filesystem that streams textures to X-Plane.

## Requirements

- macOS 12 or later (tested on Apple Silicon; Intel Macs should work but are untested)
- [macFUSE](https://macfuse.io) 5.x or later
- X-Plane 12

## Installing macFUSE

macFUSE ships as a kernel extension (kext), so installation involves a one-time
security approval that is stricter on Apple Silicon Macs.

### 1. Install the package

Download the installer from [macfuse.io](https://macfuse.io) and run it, or use Homebrew:

```bash
brew install --cask macfuse
```

### 2. Approve the kernel extension

**Apple Silicon Macs** must first allow third-party kernel extensions:

1. Shut down the Mac completely.
2. Press and **hold** the power button until "Loading startup options" appears, then choose **Options** to boot into Recovery.
3. Open **Utilities → Startup Security Utility**, select your startup disk, and choose **Reduced Security** with **"Allow user management of kernel extensions from identified developers"** checked.
4. Restart back into macOS.

**All Macs** then approve the extension:

1. Open **System Settings → Privacy & Security**.
2. Under Security you will see a message that system software from developer **"Benjamin Fleischer"** (the macFUSE developer) was blocked — click **Allow** (macOS may ask you to **Enable System Extensions** and restart).
3. Restart when prompted.

After the restart, verify macFUSE is active — the mount helper should exist:

```bash
ls /usr/local/lib/libfuse* /Library/Filesystems/macfuse.fs 2>/dev/null
```

If the approval prompt never appeared, re-run the macFUSE installer after completing the Recovery steps.

### 3. Install XEarthLayer

#### With Homebrew (Apple Silicon, recommended)

```bash
brew install --cask xearthlayer/tap/xearthlayer
```

That installs the latest stable release and `brew upgrade` keeps it current.
To follow the pre-release channel (dev, alpha, beta and rc builds) instead:

```bash
brew install --cask xearthlayer/tap/xearthlayer@prerelease
```

The two casks conflict, so switching channels means uninstalling one first.

The package publishing tool has its own casks, `xearthlayer/tap/xearthlayer-publish` and
`xearthlayer/tap/xearthlayer-publish@prerelease`. It does not need macFUSE, and installs
alongside either XEarthLayer cask.
Both depend on the `macfuse` cask; if macFUSE is already installed from its
own installer, add `--skip-cask-deps`. The macFUSE approval steps above still
apply. Then run `xearthlayer setup`.

Homebrew asks nothing extra for a third-party tap when the cask is named in
full as above: the fully qualified name trusts that one cask and no more (see
[Tap Trust](https://docs.brew.sh/Tap-Trust)). Homebrew applies its download
quarantine to every cask, which is why the binaries are signed and notarized.

#### From the installer package (Apple Silicon)

Each release carries `xearthlayer-v<version>-arm64-macos.pkg`. Download it from the
release, double-click it and follow the Installer. It puts `xearthlayer` in
`/usr/local/bin`, which is already on the `PATH`. The package is signed with the
project's Developer ID Installer certificate and notarized, with the ticket
stapled to it, so it installs without a network connection. Then run:

```bash
xearthlayer --version
xearthlayer setup
```

**Updating.** Open the newer package. It replaces the command in place.

**Removing.**

```bash
sudo rm /usr/local/bin/xearthlayer
sudo pkgutil --forget net.defreyssi.xearthlayer
```

**Checking the download.**

```bash
pkgutil --check-signature xearthlayer-v<version>-arm64-macos.pkg   # Developer ID Installer: Sam de Freyssinet (Y7DVMR5MEX)
spctl --assess --type install -vv xearthlayer-v<version>-arm64-macos.pkg   # source=Notarized Developer ID
```

**Use one method.** Homebrew installs to `/opt/homebrew/bin`, which comes before
`/usr/local/bin` on the `PATH`, so with both installed the Homebrew copy wins.
`which -a xearthlayer` lists every copy; remove the one you do not use.

The publishing tools have their own package,
`xearthlayer-publish-v<version>-arm64-macos.pkg` (receipt
`net.defreyssi.xearthlayer-publish`).

#### From a release tarball (Apple Silicon)

The manual route: each release also carries an `arm64-macos.tar.gz` tarball for Apple Silicon Macs.
There is no Intel build; on an Intel Mac, build from source as below.

The binaries are signed with the project's Apple Developer ID and notarized by
Apple, so macOS runs them as downloaded. The first run of a browser download
needs a network connection: a bare executable cannot carry its notarization
ticket, so Gatekeeper fetches it once. Pre-releases up to `v0.5.0-alpha.1` were
unsigned; see the note at the end of this section if you still have one.

Safari unpacks the tarball for you into a folder of the same name. In that
case go straight to the folder:

```bash
cd ~/Downloads/xearthlayer-<version>-arm64-macos
```

With any other browser, unpack it first:

```bash
cd ~/Downloads
tar xzf xearthlayer-<version>-arm64-macos.tar.gz
```

Then, in either case:

```bash
mkdir -p ~/.local/bin
mv xearthlayer ~/.local/bin/
xearthlayer --version
```

If `xearthlayer` is not found, `~/.local/bin` is not on your `PATH`. Add
`export PATH="$HOME/.local/bin:$PATH"` to `~/.zshrc` and open a new terminal.

To confirm what you downloaded is the project's own build:

```bash
codesign --display --verbose=2 xearthlayer   # Authority=Developer ID Application: Sam de Freyssinet (Y7DVMR5MEX)
```

**Unsigned pre-releases (`v0.5.0-alpha.1` and earlier).** Running one shows
**"xearthlayer" Not Opened: Apple could not verify "xearthlayer" is free of
malware**. Click **Done**, not Move to Trash, then clear the download
quarantine once with `xattr -c xearthlayer` before moving it into place. A
tarball fetched with `curl` instead of a browser is never quarantined.

#### From source

Building from source works exactly as on Linux:

```bash
make release
make install   # installs to ~/.local/bin
```

#### Then

```bash
xearthlayer setup
```

The setup wizard auto-detects your X-Plane installation, total memory (via `sysctl`), and storage type (via `diskutil`) to suggest sensible cache settings.

## Platform Behavior Notes

### Texture caching uses the kernel page cache

On Linux, XEarthLayer serves generated DDS textures with direct I/O so every
read from X-Plane is visible to the streaming service. On macOS, direct I/O
interacts badly with X-Plane's memory-mapped texture loading (it can crash the
simulator), so generated textures are served through the normal kernel page
cache instead.

This is safe and transparent, with one side effect: once a texture is cached by
the kernel, repeat reads never reach XEarthLayer, so the FUSE read counters on
the dashboard count each texture's first read only and under-report on macOS.
Nothing else depends on seeing every read. Aircraft position comes from the
X-Plane **Web API** on every platform. The connection is automatic and requires
no configuration, just make sure X-Plane's Web API is not disabled. Prefetching
and adaptive scenery loading work normally through it.

### Texture encoding stays on the CPU

Apple Silicon has one GPU, shared with X-Plane's Metal renderer, and no
Apple-native BC1/BC3 encoder. XEarthLayer therefore encodes DDS textures with
the ISPC backend by default (`texture.compressor = ispc`), which ships prebuilt
Apple Silicon kernels. Setting `texture.compressor = gpu` is honoured, but
XEarthLayer logs a warning at startup and the dashboard's Status pane marks the
Tile Processing Engine line with an amber `!`, because encoding would then compete with the simulator for
the GPU. The setup wizard never selects `gpu` on a Mac.

### The mount's name in Finder

Finder shows a mounted volume's name in place of the folder it is mounted on.
XEarthLayer names the macFUSE volume after its mount point, so the ortho mount
appears in Custom Scenery as `zzXEL_ortho`, the same name X-Plane and the
terminal see. Versions before 0.5.0 gave no volume name, and Finder showed
macFUSE's default, `macFUSE Volume 0 (xearthlayer)`, although the folder on
disk was always `zzXEL_ortho` ([#305](https://github.com/samsoir/xearthlayer/issues/305)).

### Stale mount recovery

If XEarthLayer is killed without unmounting (crash, force-quit), macFUSE leaves
a stale mount behind and the next run would fail with *"Device not configured
(os error 6)"*. XEarthLayer detects this at startup and force-unmounts the
stale mountpoint automatically. If you ever need to do it manually:

```bash
umount "/path/to/X-Plane 12/Custom Scenery/zzXEL_ortho"
```

### Reading a mount over SSH requires Full Disk Access

macOS applies privacy controls (TCC) per *responsible process*. Over SSH your
commands are parented to `sshd`, not to Terminal, and `sshd` has no Full Disk
Access by default. A FUSE mount counts as an external volume, so reads through
it are denied.

The symptom is misleading: the **mount succeeds**, then reads fail with
`Operation not permitted` (`EPERM`). That reads like a filesystem bug in
XEarthLayer rather than a policy decision by the OS, so it is worth checking
before debugging anything else. XEarthLayer's own read path never returns
`EPERM`; it maps every internal failure to `EIO` or `ENOENT`, so an `EPERM`
always comes from above us.

Two ways round it:

- **Run locally.** Sit at the machine, or use Screen Sharing, and run from
  Terminal there. Nothing else is needed.
- **Grant `sshd` Full Disk Access.** System Settings → Privacy & Security →
  Full Disk Access → **+**, then press `Cmd+Shift+G` and enter
  `/usr/sbin/sshd`, since the file picker hides `/usr/sbin`. On some macOS
  versions the entry is `/usr/libexec/sshd-keygen-wrapper` instead. Disconnect
  and reconnect afterwards, because the entitlement is evaluated when the
  session starts.

This matters for releases: `make verify-macos` runs the live macFUSE smoke
tests, so it cannot be completed over SSH without the above.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `mount_macfuse: the file system is not available (255)` | macFUSE kext not approved — redo the approval steps above |
| "System extension blocked" notification | System Settings → Privacy & Security → Allow, then restart |
| No approval option appears (Apple Silicon) | Boot into Recovery and enable Reduced Security with kernel extension management first |
| "Device not configured (os error 6)" on mount | Stale mount from a previous run — recovered automatically at startup; manual fix: `umount <mountpoint>` |
| macFUSE stops working after a macOS upgrade | Re-run the macFUSE installer and re-approve the extension |
| Mount succeeds but reads fail with `Operation not permitted` (`EPERM`) | You are probably over SSH. See [Reading a mount over SSH](#reading-a-mount-over-ssh-requires-full-disk-access) |
