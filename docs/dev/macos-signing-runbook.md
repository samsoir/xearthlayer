# macOS Signing Runbook

This runbook documents what XEarthLayer needs to sign and notarize its macOS
binaries, how those credentials were created, where they live, and the
procedures for a release, a renewal and a compromise.

## Overview

Every macOS binary XEarthLayer ships, on the pre-release channel as much as the
stable one, is signed with a Developer ID Application certificate and notarized
by Apple. Each release also carries an installer package per product, signed with
a Developer ID Installer certificate, notarized and stapled. Without that, a browser download is quarantined and Gatekeeper refuses
to run it until the user clears the mark by hand (`xattr -c`), which is what
every release up to `v0.5.0-alpha.2` asked of macOS testers.

The pipeline is two scripts, `scripts/macos/sign-and-notarize.sh` (the binaries)
and `scripts/macos/build-pkg.sh` (the installer packages), with shared helpers in
`scripts/macos/lib.sh`. The `package-macos` job in `release.yml` runs them on
every tag; for a release built by hand run `make sign-macos` and then
`make pkg-macos`. The helpers are tested by `make test-scripts` (part of
`make verify`). A release needs nothing manual once the credentials
below exist; the procedures in this runbook are for setting them up, renewing
them and diagnosing a failure.

> For how the release pipeline is structured, see [CI/CD Pipeline](cicd.md); for
> the release procedure itself, see the
> [Application Release Runbook](app-release-runbook.md). This runbook covers the
> Apple side only.

## Prerequisites

What is needed, by role:

| Need | Who or what | Why |
|------|-------------|-----|
| Apple Developer Program membership, active | The project's Apple account | Developer ID certificates and the notary service are membership features; a lapsed membership blocks notarization even though existing signatures keep working |
| The **Account Holder** of that account | The person doing the one-time setup | Only the Account Holder can create a Developer ID certificate. Admins cannot |
| A Mac with Xcode or the Command Line Tools | The release Mac, and the `macos-15` runner | `codesign`, `security`, `ditto` and `xcrun notarytool` |
| OpenSSL 3 (`brew install openssl`) | The release Mac, setup only | Generates the key, the signing request and the `.p12`. LibreSSL works without `-legacy` |
| `gh` CLI authenticated with admin on `samsoir/xearthlayer` | The release Mac, setup only | `gh secret set` |
| A password manager | The maintainer | The `.p12`, its password and the `.p8` must survive the release Mac |
| Network access at first run | Every user | A bare executable carries no stapled ticket; Gatekeeper fetches it once (see below) |

Checklist for a working setup (each item is produced by the numbered steps in
**First-Time Setup**):

- [ ] `security find-identity -v -p codesigning` lists one valid
      `Developer ID Application: Sam de Freyssinet (Y7DVMR5MEX)` on the release Mac
- [ ] `security find-identity -v -p basic` lists one valid
      `Developer ID Installer: Sam de Freyssinet (Y7DVMR5MEX)` on the release Mac
- [ ] `xcrun notarytool history --keychain-profile xearthlayer` answers without error
- [ ] `gh secret list -R samsoir/xearthlayer` shows all seven `APPLE_*` secrets
- [ ] The `.p12`, its password and `AuthKey_<KEYID>.p8` are in the password manager,
      and the PEM private key is deleted from disk
- [ ] The Application certificate's expiry (currently **22 November 2027**) is in a calendar
- [ ] The Installer certificate's expiry is in a calendar. Expires on: (record the
      date from `openssl x509 -noout -enddate` when the certificate is created)

## The Pieces

| Piece | What it is | Where it lives |
|-------|------------|----------------|
| Developer ID Application certificate | Signs software distributed outside the App Store. One certificate signs every channel. | Login keychain of the release Mac; `.p12` in the password manager; base64 of the `.p12` in the `APPLE_DEVELOPER_ID_P12` secret |
| Developer ID Installer certificate | Signs the `.pkg` installers. A separate certificate type from the Application one, with its own key. | Login keychain of the release Mac; `.p12` in the password manager; base64 of the `.p12` in the `APPLE_DEVELOPER_INSTALLER_P12` secret |
| App Store Connect API key | Authenticates `notarytool`. Non-interactive, no two-factor prompt, revocable on its own. | `.p8` in the password manager; its contents in the `APPLE_NOTARY_KEY_P8` secret; a `notarytool` keychain profile on the release Mac |
| Team ID | Ten characters, part of the certificate's name: `Developer ID Application: Name (TEAMID)` | Membership details at developer.apple.com |

Certificate types we deliberately do **not** use:

- **Apple Development** and **Apple Distribution** are for the App Store and
  TestFlight. Gatekeeper does not accept them for software downloaded from a
  website.

Only the **Account Holder** of the developer account can create a Developer ID
certificate; an Admin cannot. A team may hold at most five Developer ID
Application certificates. Apple sets the validity period; **the current
Application certificate expires on 22 November 2027**, so plan the renewal (below)
for autumn 2027. The Installer certificate has its own expiry, recorded in the
checklist above.

## What the Signature Looks Like

```bash
codesign --force --timestamp --options runtime --sign "Developer ID Application: ..." xearthlayer
```

- `--force` replaces the ad-hoc, linker-generated signature every Rust build
  carries on Apple Silicon.
- `--options runtime` turns on the hardened runtime, which notarization
  requires. The binaries need **no entitlements**: they link only Apple
  frameworks (Metal, Foundation, CoreGraphics and libSystem), and the pinned
  `fuse3` fork reaches macFUSE by launching `mount_macfuse` as a child process,
  never by loading a library. If a future dependency `dlopen`s a third-party
  library, signing will keep working and the binary will fail at run time with
  a library-validation error; that is the moment to add an entitlements file.
- `--timestamp` attaches a secure timestamp from Apple's server. It is what
  keeps a signature valid after the certificate expires, so a build signed
  today still runs in six years.

Notarization is one `notarytool submit --wait` per release, with both binaries
in one zip archive built by `ditto`. The script then does what a user's Mac
will do: it copies each binary, marks the copy with `com.apple.quarantine` as a
browser download would be, and runs it with `--version`. Gatekeeper assesses
the first exec of a quarantined binary, fetches the ticket, and lets it run;
without a ticket it is killed (exit 137). A release whose ticket is not being
served is a release that will not run, so that is a hard failure. The check is
skipped, with a note, on a host whose assessments are disabled.

`spctl --assess --type execute` and `syspolicy_check distribution` cannot do
this job: both understand bundles, disk images and packages only, and reject a
notarized bare executable ("does not seem to be an app", "Notary Ticket
Missing"). `codesign -R=notarized` is satisfied only by a stapled ticket.

**No stapling of the binaries.** `stapler` attaches the notarization ticket to a
bundle, disk image or installer package; a bare executable has nowhere to carry
it. A quarantined tarball copy therefore needs network access the first time it
runs, when Gatekeeper fetches the ticket, and an offline first run on a fresh
download will be refused. The installer package removes that caveat: it carries
its own stapled ticket, and the Installer does not quarantine the files it
writes.

## The Installer Package

`scripts/macos/build-pkg.sh <product> <binary> <version> <out.pkg>` builds one
package from an already signed and notarized binary. The package installs
`/usr/local/bin/<product>` under the receipt `net.defreyssi.<product>`, has no
install scripts, and targets arm64 and macOS 12.0 or later. The runtime
package shows `pkg/macos/welcome.html` in the Installer; the publisher package
has no welcome page. The script signs it with the
Developer ID Installer identity, submits it for notarization, staples the ticket
and checks the result. `xcrun stapler validate` gates in every case. The
`spctl --assess --type install` check is skipped, with a note, on a host whose
Gatekeeper assessments are disabled, as `sign-and-notarize.sh` does for its
quarantined run.

Stapling is tried up to three times, `MACOS_STAPLE_RETRY_DELAY` seconds apart
(default 15), because the ticket can lag behind `Accepted`.

## First-Time Setup (Golden Path)

Steps 1 to 4 and the Application secrets of step 6 were done once for the
project, on 22 October 2026; step 5 is pending until the Installer certificate is
created. It is
written down so the certificate can be renewed and the secrets re-created
without rediscovering it. Each step produces one item of the checklist above.

### 1. Private key and signing request

Generated locally, never on the portal, so the private key exists in exactly
one place:

```bash
mkdir -p ~/Developer/xearthlayer-signing && chmod 700 ~/Developer/xearthlayer-signing
cd ~/Developer/xearthlayer-signing
umask 077
openssl req -new -newkey rsa:2048 -nodes \
  -keyout developer_id_application.key.pem \
  -out DeveloperIDApplication.certSigningRequest \
  -subj "/emailAddress=<account email>/CN=<account holder name>"
```

Apple requires RSA 2048 for a signing request. Keychain Access's Certificate
Assistant produces the same request with the key already in the keychain; the
`openssl` route is used here because the next steps are then scriptable.

### 2. The certificate (portal, Account Holder)

At [developer.apple.com/account/resources/certificates/add](https://developer.apple.com/account/resources/certificates/add):

1. Choose **Developer ID Application** (under *Software*), continue.
2. Profile type: **G2 Sub-CA (Xcode 11.4.1 or later)**.
3. Upload `DeveloperIDApplication.certSigningRequest`, continue.
4. Download `developerID_application.cer`.

The certificate's common name is `Developer ID Application: <name> (<TEAMID>)`.

### 3. Build the identity and install it

Apple's **Developer ID Certification Authority (G2)** intermediate is included
in the `.p12`, so a bare CI runner can build the chain without fetching
anything:

```bash
cd ~/Developer/xearthlayer-signing
curl -fsSLO https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer
openssl x509 -inform der -in developerID_application.cer -out developer_id_application.pem
openssl x509 -inform der -in DeveloperIDG2CA.cer -out DeveloperIDG2CA.pem
openssl x509 -in developer_id_application.pem -noout -subject -enddate     # check name and expiry
openssl pkcs12 -export -legacy \
  -inkey developer_id_application.key.pem \
  -in developer_id_application.pem \
  -certfile DeveloperIDG2CA.pem \
  -name "Developer ID Application: <name> (<TEAMID>)" \
  -out xearthlayer-developer-id.p12          # prompts for the export password
security import xearthlayer-developer-id.p12 -k ~/Library/Keychains/login.keychain-db -T /usr/bin/codesign
security find-identity -v -p codesigning     # must list the identity as valid
```

`-legacy` makes OpenSSL 3 write the PKCS#12 encryption that `security import`
reads on every supported macOS; drop the flag with LibreSSL, which never
changed its default.

Afterwards: put `xearthlayer-developer-id.p12` and its password in the password
manager, then **delete the PEM key from disk**. The `.p12` (encrypted) is the
only copy that needs to survive, and it is what rebuilds the GitHub secret or
installs the identity on another Mac.

### 4. The notarization key (App Store Connect)

At App Store Connect, **Users and Access → Integrations → App Store Connect API
→ Team Keys**, generate a key named `xearthlayer-notary` with the **Developer**
role (the lowest role `notarytool` accepts). Record the **Issuer ID** shown at
the top of the page and the key's **Key ID**, and download `AuthKey_<KEYID>.p8`.
Apple offers the download **once**.

Save a profile for local releases, then store the `.p8` in the password manager
and delete it from disk:

```bash
xcrun notarytool store-credentials xearthlayer \
  --key ~/Downloads/AuthKey_<KEYID>.p8 --key-id <KEYID> --issuer <ISSUER-UUID>
xcrun notarytool history --keychain-profile xearthlayer     # proves the key works
```

### 5. Developer ID Installer certificate

Pending: not yet done. Account Holder only. Repeat steps 1 to 3 with the Installer certificate type:

1. Generate a new key and request with the commands of step 1, using new file
   names (`developer_id_installer.key.pem`,
   `DeveloperIDInstaller.certSigningRequest`).
2. On the portal choose **Developer ID Installer** (under *Software*), profile
   type **G2 Sub-CA**, upload the request and download the certificate
   (`developerID_installer.cer`).
3. Build the `.p12` as in step 3 with the same G2 intermediate, from
   `developer_id_installer.pem` and `developer_id_installer.key.pem`, named
   `Developer ID Installer: <name> (<TEAMID>)`, written to
   `xearthlayer-developer-id-installer.p12`. Without the intermediate,
   `security find-identity -v -p basic` shows the identity as invalid and the
   script reports no valid identity.
4. Import it into the login keychain, allowing the packaging tools to use it:

   ```bash
   security import xearthlayer-developer-id-installer.p12 -k ~/Library/Keychains/login.keychain-db -T /usr/bin/productbuild -T /usr/bin/productsign
   security find-identity -v -p basic     # must list the Installer identity as valid
   ```

   An Installer identity is not a code signing identity, so it appears under
   `-p basic` and not under `-p codesigning`.
5. Put the `.p12` and its password in the password manager and delete the PEM key.
   Record the expiry from `openssl x509 -in developer_id_installer.pem -noout
   -enddate` in the checklist above.

### 6. GitHub secrets

| Secret | Value |
|--------|-------|
| `APPLE_DEVELOPER_ID_P12` | `base64 -i xearthlayer-developer-id.p12` |
| `APPLE_DEVELOPER_ID_P12_PASSWORD` | the export password |
| `APPLE_DEVELOPER_INSTALLER_P12` | `base64 -i xearthlayer-developer-id-installer.p12` |
| `APPLE_DEVELOPER_INSTALLER_P12_PASSWORD` | the Installer export password |
| `APPLE_NOTARY_KEY_P8` | the contents of `AuthKey_<KEYID>.p8` |
| `APPLE_NOTARY_KEY_ID` | the key ID |
| `APPLE_NOTARY_ISSUER_ID` | the issuer ID |

```bash
gh secret set APPLE_DEVELOPER_ID_P12 -R samsoir/xearthlayer < <(base64 -i xearthlayer-developer-id.p12)
gh secret set APPLE_DEVELOPER_ID_P12_PASSWORD -R samsoir/xearthlayer     # prompts
gh secret set APPLE_DEVELOPER_INSTALLER_P12 -R samsoir/xearthlayer < <(base64 -i xearthlayer-developer-id-installer.p12)
gh secret set APPLE_DEVELOPER_INSTALLER_P12_PASSWORD -R samsoir/xearthlayer     # prompts
gh secret set APPLE_NOTARY_KEY_P8 -R samsoir/xearthlayer < AuthKey_<KEYID>.p8
gh secret set APPLE_NOTARY_KEY_ID -R samsoir/xearthlayer                  # prompts
gh secret set APPLE_NOTARY_ISSUER_ID -R samsoir/xearthlayer               # prompts
```

The workflow signs only when `APPLE_DEVELOPER_ID_P12` is set; a fork, or this
repository before the secrets existed, still produces an unsigned tarball. The
Application and notary secrets are read together, so set those five in one sitting: a certificate
without notary credentials fails the job rather than shipping a signed but
un-notarized binary that Gatekeeper would still refuse.

Without `APPLE_DEVELOPER_INSTALLER_P12` the workflow still signs and notarizes the
tarballs, emits a `::notice::` and builds no packages. A set `.p12` with a missing
password fails at import. `version.json` already
lists `macos_pkg` and `publish_macos_pkg`, so until `APPLE_DEVELOPER_INSTALLER_P12` is set
a release names packages it does not carry. The website ignores those keys today.

## Releasing

### In the release workflow

`package-macos` (see [cicd.md](cicd.md)) gained steps between the build and the
tarball, gated on `env.MACOS_SIGNING` and `env.MACOS_PKG` (the secrets context is
not readable from a step's `if:`, so the job computes the flags once):

1. **Import Developer ID certificate**: decodes the `.p12` into a keychain
   created for the job under `$RUNNER_TEMP`, grants `codesign` access to the key
   without a UI prompt (`set-key-partition-list`), and prepends the keychain to
   the search list. When the Installer secret is set it imports a second
   identity, the Developer ID Installer `.p12`, into the same keychain, open to
   `productbuild` and `productsign`. The runner's login keychain is never touched.
2. **Sign and notarize**: writes the `.p8` to a temporary file and runs the
   script on both binaries.
3. **Build installer packages** (when both `.p12` secrets are set): writes the
   `.p8` to a temporary file and runs `build-pkg.sh` for `xearthlayer` and
   `xearthlayer-publish`, producing `dist/<product>-vX.Y.Z-arm64-macos.pkg`. If
   the Installer secret is absent, **Note missing installer identity** emits the
   notice instead.
4. **Remove signing keychain**: `always()`, so a failed notarization does not
   leave the identities on the runner (hosted runners are discarded anyway; the
   step exists for self-hosted ones). It runs after the packages are built.

Notarization adds a few minutes to the job, typically two to five, and each
package is notarized separately, a few minutes more per package.

### By hand

```bash
make release
MACOS_NOTARY_PROFILE=xearthlayer make sign-macos
MACOS_NOTARY_PROFILE=xearthlayer make pkg-macos
```

`make pkg-macos` packages the binaries `make sign-macos` just signed, so run it
second. It finds the single valid Developer ID Installer identity in the
keychain; set `MACOS_INSTALLER_IDENTITY` to a name when there is more than one.
`MACOS_SKIP_NOTARIZE=1` signs the packages without submitting them.

The script finds the single valid Developer ID Application identity in the
keychain; set `MACOS_SIGN_IDENTITY` to a name or SHA-1 hash when there is more
than one. `MACOS_SKIP_NOTARIZE=1` signs without submitting, for a quick check
that the identity works.

### Checking a shipped binary

```bash
codesign --display --verbose=2 xearthlayer        # Authority=Developer ID Application: ..., flags=...(runtime), Timestamp=...
cp xearthlayer /tmp/probe && xattr -w com.apple.quarantine "0083;$(printf '%x' $(date +%s));Safari;$(uuidgen)" /tmp/probe && /tmp/probe --version
```

The second line is the notarization check: a quarantined copy runs only when
Gatekeeper finds the ticket. The realistic test is to download the tarball in
Safari on a Mac that has never run XEarthLayer and run the binary from the
unpacked folder. It should start with no dialog.

### Checking a shipped package

```bash
pkgutil --check-signature xearthlayer-v<version>-arm64-macos.pkg   # Developer ID Installer: ..., with a timestamp
spctl -a -vv -t install xearthlayer-v<version>-arm64-macos.pkg     # source=Notarized Developer ID
xcrun stapler validate xearthlayer-v<version>-arm64-macos.pkg      # The validate action worked!
```

## Renewal, Rotation, Revocation

- **Expiry.** Check with `openssl x509 -in developer_id_application.pem -noout
  -enddate`; the current certificate ends 22 November 2027. Binaries
  signed with a timestamp keep running after expiry. Renewal is steps 1 to 3
  and 6 above with a new key; the old certificate need not be revoked.
  The Installer certificate renews the same way, with step 5. A package signed
  with a timestamp keeps installing after the certificate expires. While both the
  old and the new Installer certificate are in the keychain,
  `MACOS_INSTALLER_IDENTITY` (a name) matches two identities, so delete the old
  one from the keychain after the first release signed with the new one. Whether
  `productbuild` accepts a SHA-1 hash in its place is unconfirmed; leave it to
  the Mac check.
- **Leaked `.p12`.** Revoke the certificate on the portal, then renew. **A
  revoked Developer ID certificate invalidates every build it signed**, and
  Gatekeeper will refuse already-published releases, so revoke only for an
  actual compromise and follow it with a re-signed release.
- **Leaked `.p8`.** Revoke the key under Team Keys and generate a new one. This
  affects nothing already notarized.
- **New release Mac.** Import the `.p12` from the password manager and save the
  `notarytool` profile again; nothing else is machine-specific.

## Troubleshooting

| Symptom | Cause |
|---------|-------|
| `no valid 'Developer ID Application' identity` | The certificate is absent, expired, or its chain does not build: `security find-identity -p codesigning` (without `-v`) shows it, `-v` does not. Re-create the `.p12` with the G2 intermediate (step 3). |
| `no valid 'Developer ID Installer' identity` | The certificate is absent, expired, its chain does not build (re-create the `.p12` with the G2 intermediate, step 5), or it was looked up with `-p codesigning`, which never lists an Installer identity: use `security find-identity -v -p basic`. |
| `spctl` says `Unnotarized Developer ID` after `Accepted` | The staple did not run, or the package changed after it was submitted. The script retries `stapler staple` three times, `MACOS_STAPLE_RETRY_DELAY` seconds apart (default 15), since the ticket can lag behind `Accepted`; rebuild the package rather than editing it. |
| `errSecInternalComponent` from `codesign` in CI | The key's partition list was not set, or the keychain locked. Both are handled by the import step; check it ran on this job. |
| `status: Invalid` from `notarytool` | The job prints `notarytool log <id>`; the usual entries are a missing timestamp or hardened runtime, which the script always sets, so look for a new binary or dependency. |
| Quarantined run killed right after `Accepted` | Ticket propagation; the script retries three times ten seconds apart. Persisting, check `notarytool info <id>`, and whether the signed bytes changed after submission (a re-sign changes nothing: the ticket is keyed by the code directory hash, which the timestamp is not part of). |
| User reports *"xearthlayer" Not Opened* | Either a pre-`0.5.0` build, or a first run with no network (no stapled ticket, see above). |
