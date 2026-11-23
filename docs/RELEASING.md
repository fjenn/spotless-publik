# Releasing Spotless

Mac releases are below; iPhone (TestFlight) builds are in [their own section](#iphone-testflight).

Mac releases are built, signed with the Developer ID certificate, notarized, stapled and published
by GitHub Actions ([`.github/workflows/release-mac.yml`](../.github/workflows/release-mac.yml))
whenever a `v*` tag is pushed. No Mac needs to be on. The workflow runs
[`scripts/release-mac.sh`](../scripts/release-mac.sh), the same script used for local releases,
and attaches to the GitHub Release:

| Asset | What it is for |
|---|---|
| `Spotless-<version>.dmg` | the installer for this version |
| `Spotless.dmg` | the same file under a fixed name; the website's download button points at `releases/latest/download/Spotless.dmg` |
| `Spotless_<version>_universal.app.tar.gz` + `.sig` | the signed update the in-app updater installs |
| `latest.json` | what installed copies poll (`releases/latest/download/latest.json`) to find an update |

The release is created as a draft, filled with every asset, and only then published, so
`latest.json` never points at a file that is not there yet. Tags with a suffix (`v1.1.0-beta.1`)
become pre-releases, which `releases/latest` (and so the updater) skips.

## One-time setup (owner)

1. **Have an App Store Connect API key** for notarization (the `AuthKey_<KEYID>.p8` file). If
   there is none yet (account holder or admin): App Store Connect › Users and Access ›
   Integrations › App Store Connect API › Team Keys › **+**, name it e.g. "Spotless
   notarization", access **Developer**, Generate, and download the `.p8` (Apple allows this
   once). Either way, copy the **Issuer ID** shown above the keys table.
2. **Run the secrets script** on the Mac whose login keychain has the "Developer ID Application"
   certificate, with `gh` logged in, giving it the key file:
   ```bash
   scripts/ci/setup-github-secrets.sh ~/Desktop/AuthKey_<KEYID>.p8   # --repo owner/name for another repo
   ```
   It asks which Developer ID identity to use, a password for the exported .p12 (Enter for a
   random one), confirms the Key ID it read from the file name, asks the Issuer ID (it checks the
   key with Apple), and the updater key (`~/.tauri/spotless-updater.key`, plus its password if it
   has one). macOS shows a
   keychain dialog to allow the private-key export: click **Allow**. Nothing is printed; it ends
   with the list of secret names it set. Re-run it any time (new certificate, rotated key).
3. **Push the tag** for the version that is in `src-tauri/tauri.conf.json` and `Cargo.toml`, in
   the public repo's checkout (the snapshot that `scripts/publish-snapshot.sh` writes and that is
   pushed to `fjenn/spotless`; the tag has to point at a commit there):
   ```bash
   cd ~/music-app/public-snapshot      # after publish-snapshot.sh and `git push origin main`
   git tag -a v1.0.0 -m "Spotless 1.0.0"
   git push origin v1.0.0
   ```
   Then watch Actions › Release macOS (about 30-60 minutes; notarization is usually a few of them).

## Every release after that

1. Bump `version` in `src-tauri/tauri.conf.json` and `[workspace.package]` in `Cargo.toml`
   (the workflow fails if either differs from the tag), commit, run `scripts/publish-snapshot.sh`
   and push the snapshot's `main`.
2. Release notes, first found wins: `RELEASE_NOTES.md` at the repo root if it mentions the new
   version; the annotated tag's message (`git tag -a v1.0.1 -m "Spotless 1.0.1" -m "- Fixed …"`;
   the lines after the first become the notes); otherwise GitHub's generated notes. The same text
   shows in the app's update prompt.
3. Push the tag `v<version>` from the snapshot checkout. Or, from Actions › Release macOS › Run workflow, enter the tag: an
   existing tag is rebuilt and its assets replaced; a new one is created on the chosen branch.

## Secrets

| Secret | Contents |
|---|---|
| `MACOS_CERTIFICATE` | base64 of the Developer ID Application identity (.p12, certificate + private key) |
| `MACOS_CERTIFICATE_PASSWORD` | that .p12's password |
| `APPLE_API_KEY` | base64 of the App Store Connect API key (`AuthKey_….p8`) |
| `APPLE_API_KEY_ID` | its Key ID |
| `APPLE_API_ISSUER_ID` | its Issuer ID |
| `TAURI_SIGNING_PRIVATE_KEY` | contents of the updater key file |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | optional, only if that key has a password |

The workflow stops at its first step if one of the required ones is missing. In the job the
certificate lives in a temporary keychain with a random password, the key files in the runner's
temp folder; both are deleted at the end, also when the run fails. The signing identity is found
in that keychain, so its hash is not a secret.

## Local release (still works)

`scripts/release-mac.sh` with `SIGN_IDENTITY` and `NOTARY_PROFILE` set, as before (the script
header has the details). It also accepts `NOTARY_KEY_PATH` / `NOTARY_KEY_ID` / `NOTARY_ISSUER`
(the API key) instead of a profile. Output goes to `../dist`; upload the files it lists by hand.

## If a run fails

- **Version mismatch**: bump both version fields, commit, move the tag (`git tag -f -a v…` and
  `git push -f origin v…`) or tag a new version.
- **Notarization not accepted**: the log Apple returns is printed right above the error.
- **No valid identity**: the certificate expired or the .p12 lacks the private key; re-run the
  secrets script.
- **DMG without background**: the runner's Finder did not answer three times; the DMG still works
  (app + Applications link). Re-run the job for the styled one.

## iPhone (TestFlight)

[`.github/workflows/release-ios.yml`](../.github/workflows/release-ios.yml) builds the iPhone app
(`ios/Encore`, bundle `app.spotlessmusic.ios`) and uploads it to App Store Connect whenever an
`ios-v*` tag is pushed. Apple then processes it (usually 5-30 minutes) and it shows up in
TestFlight. The same archive, unsigned, is also published for sideloading (below); the run
summary names the version and build number.

What the job does (macOS 26 runner, newest stable Xcode, roughly 30-60 minutes; the Rust core in
release mode with LTO is the slow part):

1. checks the tag against `ios/Encore/project.yml` (`ios-v<MARKETING_VERSION>-<CURRENT_PROJECT_VERSION>`);
2. builds the Rust core for the device only (`scripts/build-ios-core.sh --release --device`);
3. writes the team ID into `project.yml` (the public snapshot blanks it) and runs `xcodegen`;
4. `xcodebuild archive` and `xcodebuild -exportArchive` with
   [`ios/Encore/ExportOptions.plist`](../ios/Encore/ExportOptions.plist) (method
   `app-store-connect`, destination `upload`, automatic signing), both with
   `-allowProvisioningUpdates` and the App Store Connect API key, so Xcode manages the
   certificates and profiles and the export uploads the build itself;
5. packages the archived app **unsigned** as `Spotless.ipa` (development signature and embedded
   profile stripped, `Payload/Spotless.app`) and, after the upload, publishes it on a GitHub
   **pre-release** named after the tag (see Sideloading);
6. removes the key file and the temporary keychain, also when the run fails.

No tests run in this job (unit, UI and E2E tests run locally: `ios/Encore/scripts`).

### Cutting a TestFlight build

1. In `ios/Encore/project.yml`, set `MARKETING_VERSION` (the version users see) and raise
   `CURRENT_PROJECT_VERSION` (the build number; App Store Connect refuses one it has seen for
   that version). Commit on `main`.
2. Export and push the public snapshot:
   ```bash
   scripts/publish-snapshot.sh
   git -C ~/music-app/public-snapshot push origin main
   ```
3. Tag the snapshot commit and push the tag:
   ```bash
   cd ~/music-app/public-snapshot
   git tag ios-v1.0.4-15          # ios-v<MARKETING_VERSION>-<CURRENT_PROJECT_VERSION>
   git push origin ios-v1.0.4-15
   ```
   Or Actions › Release iOS › Run workflow, with the tag (or empty, to build the chosen branch as
   it is). Mac tags (`v*`) and iPhone tags (`ios-v*`) never start each other's workflow.

### Sideloading (SideStore / AltStore)

Every `ios-v*` run creates the pre-release `ios-v<version>-<build>` on `fjenn/spotless` with
`Spotless.ipa`, `Spotless-<version>-<build>.ipa` and `altstore.json` (an AltStore source for that
one build, written by [`scripts/ci/altstore-source.rb`](../scripts/ci/altstore-source.rb) from the
IPA's own Info.plist and size). It is marked pre-release and not latest, so
`releases/latest` (the Mac DMG and its updater) never points at it.

The public source is **https://opena.tech/spotless/altstore.json** (the opena site): it reads the
newest `ios-v*` release with a `Spotless.ipa` from the GitHub API about once an hour and lists its
builds, newest first, taking the permissions and minimum iOS from that release's `altstore.json`
(AltStore refuses an install when the source's permissions differ from the IPA's). Nothing to do
per release beyond pushing the tag.

Packaging the IPA never blocks TestFlight (it may fail on its own and only warns). If publishing
fails after a successful upload, a full re-run would hit "build number already used"; instead
download the `Spotless-ipa` artifact the run keeps in that case and attach it (`gh release upload`), or bump the build and tag again.

### One-time setup

- **API key role.** The workflow uses the same `APPLE_API_KEY` / `APPLE_API_KEY_ID` /
  `APPLE_API_ISSUER_ID` secrets as the Mac release. Uploading a build works with a **Developer**
  key, but automatic signing in CI also has to create (or use the cloud-managed) **Apple
  Distribution** certificate and the App Store provisioning profile, which a Developer key is not
  allowed to do. If the archive or export step fails with a permission or "no signing
  certificate" error, create a key with **Admin** access (or **App Manager** with access to
  Certificates, Identifiers & Profiles and cloud-managed distribution certificates): App Store
  Connect › Users and Access › Integrations › Team Keys › **+**. To leave the Mac's notarization
  key as it is, store the new one separately; the iOS workflow prefers it when both are set:
  ```bash
  base64 -i ~/Downloads/AuthKey_<KEYID>.p8 | gh secret set APPLE_IOS_API_KEY --repo fjenn/spotless
  gh secret set APPLE_IOS_API_KEY_ID --repo fjenn/spotless --body <KEYID>
  ```
  (Same team, so the Issuer ID stays `APPLE_API_ISSUER_ID`.)
- **Team ID** (optional). Without it, the job reads the team from App Store Connect (the bundle
  ID's App ID prefix, `scripts/ci/asc-team-id.rb`). To pin it, add a repository **variable**
  (not a secret): Settings › Secrets and variables › Actions › Variables › `APPLE_TEAM_ID` = the
  10-character team ID, or
  `gh variable set APPLE_TEAM_ID --repo fjenn/spotless --body <TEAMID>`.

| Secret / variable | Needed | Contents |
|---|---|---|
| `APPLE_API_KEY` | yes (unless the iOS pair is set) | base64 of the `.p8`, shared with the Mac release |
| `APPLE_API_KEY_ID` | yes (unless the iOS pair is set) | its Key ID |
| `APPLE_API_ISSUER_ID` | yes | the Issuer ID |
| `APPLE_IOS_API_KEY`, `APPLE_IOS_API_KEY_ID` | optional | a separate key for iOS signing (e.g. Admin) |
| variable `APPLE_TEAM_ID` | optional | the team ID; otherwise read from App Store Connect |

`MACOS_CERTIFICATE` is the Developer ID certificate and plays no part in iOS signing.

### Fallback: a stored distribution certificate

If no Admin/App Manager key can be used, sign manually instead of with cloud signing: export the
**Apple Distribution** identity (certificate + private key) as a .p12 from the Mac that has it,
download the **App Store** provisioning profile for `app.spotlessmusic.ios`, and store both as
secrets (`IOS_DISTRIBUTION_CERTIFICATE` + `_PASSWORD`, `IOS_PROVISIONING_PROFILE`, base64). The
workflow would then import the .p12 into its temporary keychain (as release-mac.yml does), copy
the profile to `~/Library/MobileDevice/Provisioning Profiles/`, archive with
`CODE_SIGN_STYLE=Manual` for the `Encore` target, and export with `signingStyle` `manual` plus a
`provisioningProfiles` entry in the export options. This is not wired up yet, as the API-key path
needs no files that expire.

### If a run fails

- **Tag does not match project.yml**: fix the two values, commit, publish the snapshot, and tag
  again (or move the tag: `git tag -f …` and `git push -f origin <tag>`).
- **"Redundant binary upload" / build number already used**: raise `CURRENT_PROJECT_VERSION`.
- **Signing / permission errors**: the API key's role (see One-time setup). Each CI run may
  create an "Apple Development" certificate for the archive; if Apple reports too many, revoke
  old ones marked "Created via API" in Certificates, Identifiers & Profiles.
- **Logs**: the full `xcodebuild` logs are attached to a failed run as `ios-release-logs`.

## Local iPhone release (still works)

Run `scripts/build-ios-core.sh --release --device` at the repo root, then in `ios/Encore`:
`xcodebuild -scheme Encore -configuration Release -destination generic/platform=iOS -archivePath build/Spotless.xcarchive -allowProvisioningUpdates archive`,
then `xcodebuild -exportArchive -archivePath build/Spotless.xcarchive -exportOptionsPlist <copy of ExportOptions.plist with teamID set> -exportPath build/export -allowProvisioningUpdates`.

## The other Spotless repos (keep them current)

Everything starts in this working copy (private, on this Mac only). Three GitHub repos follow it:

| Repo | What it is | How it is updated |
|---|---|---|
| `fjenn/spotless` | The open-source app (Mac, iPhone, Windows, Linux) | `scripts/publish-snapshot.sh`, then push `~/music-app/public-snapshot` main |
| `fjenn/spotless-publik` | `fjenn/spotless` + the `publik/` layer (Mac, publikhq.com) | `publik-sync.yml` hourly, or `publik/sync.sh` in `~/spotless-publik` (needed by hand when a workflow file changed). Releases only with the owner's OK. |
| `fjenn/spotless-appstore` | The App Store edition (iPhone only, bundle `app.spotlessapp.ios`, no import or sign-in); its own source of truth, real history | In `~/spotless-appstore`: `scripts/appstore/sync.sh` brings fjenn/spotless's `crates/`, `ios/Encore/Encore` and `ios/EncoreCore` changes over as a PR (CI compiles the app unsigned); merge it when green. The old private mirror is backed up at `~/music-app/backups/spotless-appstore-private.git`. |

After every iPhone or core change: publish the snapshot first, then run both syncs.
