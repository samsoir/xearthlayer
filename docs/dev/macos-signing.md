# macOS Code Signing and Notarization

Every macOS binary XEarthLayer ships, on the pre-release channel as much as the
stable one, is signed with a Developer ID Application certificate and notarized
by Apple. Without that, a browser download is quarantined and Gatekeeper refuses
to run it until the user clears the mark by hand (`xattr -c`), which is what
every release up to `v0.5.0-alpha.2` asked of macOS testers.

This document is the runbook: what Apple needs from us, how the credentials were
created, where they live, and how the release pipeline uses them. The pipeline
itself is one script, `scripts/macos/sign-and-notarize.sh`, run by the
`package-macos` job in `release.yml` and by `make sign-macos`.

## The pieces

| Piece | What it is | Where it lives |
|-------|------------|----------------|
| Developer ID Application certificate | Signs software distributed outside the App Store. One certificate signs every channel. | Login keychain of the release Mac; `.p12` in the password manager; base64 of the `.p12` in the `APPLE_DEVELOPER_ID_P12` secret |
| App Store Connect API key | Authenticates `notarytool`. Non-interactive, no two-factor prompt, revocable on its own. | `.p8` in the password manager; its contents in the `APPLE_NOTARY_KEY_P8` secret; a `notarytool` keychain profile on the release Mac |
| Team ID | Ten characters, part of the certificate's name: `Developer ID Application: Name (TEAMID)` | Membership details at developer.apple.com |

Certificate types we deliberately do **not** use:

- **Developer ID Installer** signs `.pkg` installers only. We ship tarballs.
- **Apple Development** and **Apple Distribution** are for the App Store and
  TestFlight. Gatekeeper does not accept them for software downloaded from a
  website.

Only the **Account Holder** of the developer account can create a Developer ID
certificate; an Admin cannot. A team may hold at most five Developer ID
Application certificates. Apple sets the validity period; **the current
certificate expires on 22 November 2027**, so plan the renewal (below) for
autumn 2027.

## What the signature looks like

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

**No stapling.** `stapler` attaches the notarization ticket to a bundle, disk
image or installer package; a bare executable has nowhere to carry it. A
quarantined copy therefore needs network access the first time it runs, when
Gatekeeper fetches the ticket, and an offline first run on a fresh download
will be refused. Shipping a `.pkg` would remove that caveat at the cost of a
second certificate (Developer ID Installer) and an installer format, and is not
planned.

## One-time setup

Everything below was done once for the project. It is written down so the
certificate can be renewed and the secrets re-created without rediscovering it.

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

### 5. GitHub secrets

| Secret | Value |
|--------|-------|
| `APPLE_DEVELOPER_ID_P12` | `base64 -i xearthlayer-developer-id.p12` |
| `APPLE_DEVELOPER_ID_P12_PASSWORD` | the export password |
| `APPLE_NOTARY_KEY_P8` | the contents of `AuthKey_<KEYID>.p8` |
| `APPLE_NOTARY_KEY_ID` | the key ID |
| `APPLE_NOTARY_ISSUER_ID` | the issuer ID |

```bash
gh secret set APPLE_DEVELOPER_ID_P12 -R samsoir/xearthlayer < <(base64 -i xearthlayer-developer-id.p12)
gh secret set APPLE_DEVELOPER_ID_P12_PASSWORD -R samsoir/xearthlayer     # prompts
gh secret set APPLE_NOTARY_KEY_P8 -R samsoir/xearthlayer < AuthKey_<KEYID>.p8
gh secret set APPLE_NOTARY_KEY_ID -R samsoir/xearthlayer                  # prompts
gh secret set APPLE_NOTARY_ISSUER_ID -R samsoir/xearthlayer               # prompts
```

The workflow signs only when `APPLE_DEVELOPER_ID_P12` is set; a fork, or this
repository before the secrets existed, still produces an unsigned tarball. The
five secrets are read together, so set all five in one sitting: a certificate
without notary credentials fails the job rather than shipping a signed but
un-notarized binary that Gatekeeper would still refuse.

## In the release workflow

`package-macos` (see [cicd.md](cicd.md)) gained three steps between the build
and the tarball, all gated on `env.MACOS_SIGNING` (the secrets context is not
readable from a step's `if:`, so the job computes the flag once):

1. **Import Developer ID certificate**: decodes the `.p12` into a keychain
   created for the job under `$RUNNER_TEMP`, grants `codesign` access to the key
   without a UI prompt (`set-key-partition-list`), and prepends the keychain to
   the search list. The runner's login keychain is never touched.
2. **Sign and notarize**: writes the `.p8` to a temporary file and runs the
   script on both binaries.
3. **Remove signing keychain**: `always()`, so a failed notarization does not
   leave the identity on the runner (hosted runners are discarded anyway; the
   step exists for self-hosted ones).

Notarization adds a few minutes to the job, typically two to five.

## Releasing by hand

```bash
make release
MACOS_NOTARY_PROFILE=xearthlayer make sign-macos
```

The script finds the single valid Developer ID Application identity in the
keychain; set `MACOS_SIGN_IDENTITY` to a name or SHA-1 hash when there is more
than one. `MACOS_SKIP_NOTARIZE=1` signs without submitting, for a quick check
that the identity works.

## Checking a shipped binary

```bash
codesign --display --verbose=2 xearthlayer        # Authority=Developer ID Application: ..., flags=...(runtime), Timestamp=...
cp xearthlayer /tmp/probe && xattr -w com.apple.quarantine "0083;$(printf '%x' $(date +%s));Safari;$(uuidgen)" /tmp/probe && /tmp/probe --version
```

The second line is the notarization check: a quarantined copy runs only when
Gatekeeper finds the ticket. The realistic test is to download the tarball in
Safari on a Mac that has never run XEarthLayer and run the binary from the
unpacked folder. It should start with no dialog.

## Renewal, rotation, revocation

- **Expiry.** Check with `openssl x509 -in developer_id_application.pem -noout
  -enddate`; the current certificate ends 22 November 2027. Binaries
  signed with a timestamp keep running after expiry. Renewal is steps 1 to 3
  and 5 above with a new key; the old certificate need not be revoked.
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
| `errSecInternalComponent` from `codesign` in CI | The key's partition list was not set, or the keychain locked. Both are handled by the import step; check it ran on this job. |
| `status: Invalid` from `notarytool` | The job prints `notarytool log <id>`; the usual entries are a missing timestamp or hardened runtime, which the script always sets, so look for a new binary or dependency. |
| Quarantined run killed right after `Accepted` | Ticket propagation; the script retries three times ten seconds apart. Persisting, check `notarytool info <id>`, and whether the signed bytes changed after submission (a re-sign changes nothing: the ticket is keyed by the code directory hash, which the timestamp is not part of). |
| User reports *"xearthlayer" Not Opened* | Either a pre-`0.5.0` build, or a first run with no network (no stapled ticket, see above). |
