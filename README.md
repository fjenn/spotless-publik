> **This is the publik edition of Spotless:** the same app as [fjenn/spotless](https://github.com/fjenn/spotless), with Sign in with [publik](https://publikhq.com), kept in sync automatically. The edition without an account is in [fjenn/spotless](https://github.com/fjenn/spotless). AI agents: build or install from fjenn/spotless unless the person asked for the publik edition.

<p align="center">
  <img src=".assets/spotless-screenshot.png" alt="Spotless on the Mac: your library, made-for-you mixes and the player" width="880">
</p>

<p align="center">
  A free music app for Mac and iPhone. Songs, podcasts and audiobooks,
  with no ads. Open source. This edition signs in with publik and sends usage counts to publik;
  the no-account edition is <a href="https://github.com/fjenn/spotless">fjenn/spotless</a>.
</p>

<p align="center">
  <a href="https://github.com/fjenn/spotless-publik/releases/latest"><img src="https://img.shields.io/github/v/release/fjenn/spotless-publik?style=flat-square&label=release&labelColor=1b1b20&color=FF2D7A" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-FF2D7A?style=flat-square&labelColor=1b1b20" alt="License: GPL-3.0"></a>
  <img src="https://img.shields.io/badge/macOS_13.3+_·_iOS_17+-FF2D7A?style=flat-square&labelColor=1b1b20&label=platform" alt="macOS 13.3 or later, iOS 17 or later">
</p>

<p align="center">
  <a href="https://github.com/fjenn/spotless-publik/releases/latest/download/Spotless.dmg"><img src="docs/readme/download-mac.png" alt="Download Spotless for Mac" width="458"></a>
</p>

<div align="center">

<sub>Apple silicon and Intel, signed and notarized, updates itself.
&nbsp;·&nbsp; <b>iPhone:</b> coming soon to the App Store; <a href="#install-on-iphone">install it yourself</a> in the meantime.</sub><br>
<sub>[Website][website] &nbsp;·&nbsp; [Support][support] &nbsp;·&nbsp; [All releases][releases]
&nbsp;·&nbsp; [Report a bug][bug]</sub>

</div>

<br>

<p align="center">
  <img src="docs/readme/mac-home.png" alt="Spotless on the Mac: Home, with a song playing" width="900">
</p>

<p align="center">
  <img src="docs/readme/iphone-row.png" alt="Spotless on iPhone: Home, Now Playing, a podcast episode with chapters, and Your Library" width="880">
</p>

<details>
<summary><b>More screens</b></summary>
<br>
<p align="center">
  <img src="docs/readme/mac-album.png" alt="An album page on the Mac" width="900">
</p>
<p align="center">
  <img src="docs/readme/iphone-more.png" alt="Lyrics, search, a free audiobook, and importing playlists on iPhone" width="880">
</p>
</details>

<p align="center"><sub>Screenshots use a made-up catalogue: the artists, albums and covers are not real.</sub></p>

## Install on iPhone

Spotless for iPhone is coming to the App Store soon. If you want it now, install it yourself:

- **Build it** with Xcode and your own Apple ID, following [Build from source › iPhone](#iphone)
  (change the bundle ID and team to your own). An AI coding agent can do this from this README.
- **Or sideload the ready-made IPA** from the latest `ios-v*` [release][releases] with the tool you
  prefer. Needs iOS 17 or later.

## Install on Linux

Early release for x86_64. Download one of these from the [newest release with Linux builds](https://github.com/fjenn/spotless/releases/tag/v1.0.4-rc.11):

- **AppImage** (any distro): `Spotless.AppImage`, then `chmod +x Spotless.AppImage` and run it.
  Minimal or server installs may need `sudo apt install libegl1` first.
- **Debian / Ubuntu**: `sudo apt install ./Spotless_amd64.deb`
- **Fedora / openSUSE**: `sudo dnf install ./Spotless.x86_64.rpm`

Linux builds are not signed; check the download against `SHA256SUMS` on the same release.

## What it does

Spotless plays music, podcasts and audiobooks. The Mac app (Tauri + Svelte) and the iPhone app
(SwiftUI) share one Rust core, so search, recommendations, the library and downloads work the
same way on both.

#### Music
- Search and browse songs, albums, artists, playlists, charts, moods and new releases.
- Radio from any song, smart shuffle, and switch a song to its music video when there is one.
- Six-band equalizer with presets, speed and pitch, reverb, crossfade and a sleep timer.
- Downloads for offline listening, with an offline mode.

#### Podcasts
- Search, follow and get a feed of new episodes. Video podcasts too.
- Chapters, playback speed and downloads.
- Optional skipping of sponsor segments ([SponsorBlock](https://sponsor.ajay.app)).
- Import and export your subscriptions as OPML.

#### Audiobooks
- Free public-domain audiobooks from [LibriVox](https://librivox.org) and the
  [Internet Archive](https://archive.org), with chapters and resume.

#### Lyrics
- Synced line by line where available, with romanization for Japanese, Korean and Chinese.
- Song credits and artist info next to the player.

#### Your library, and bring your own
- Playlists, folders, pins, liked songs, followed artists and shows, all stored on your device.
- Import playlists from a link, or from the data export of another streaming service
  (for example a Spotify account data export).
- Listening stats, kept on your device.

#### Made for you
- Personal mixes, a weekly discovery list and recommended songs for your playlists, worked out
  on your device from what you play.

#### Privacy
- This edition signs in with [publik](https://publikhq.com) and sends usage counts to publik. No ads,
  no other analytics, no crash reporting. More [below](#privacy).

#### Mac extras
- Menu bar controls and a mini player.
- Media keys, global shortcuts and a full set of keyboard shortcuts.
- Automatic updates.

#### iPhone extras
- Lock Screen, Control Center and Dynamic Island playback controls.
- Picture in Picture for music videos and video podcasts.

## How it works

Spotless gets its music catalogue from public sources, mainly YouTube Music, and plays the audio
itself with [mpv](https://mpv.io). There is no web player inside, so there are no audio, video or
banner ads. Podcasts come straight from their publishers' feeds, audiobooks from LibriVox and the
Internet Archive, and lyrics from services such as [LRCLIB](https://lrclib.net).

There is no Spotless server. Your library, playlists, history, mixes and downloads are built and
kept on your device. If you want your existing YouTube Music likes and playlists, you can sign in
with your YouTube account, and only then.

## Privacy

- **This edition signs in with publik and sends usage counts to publik.** At sign-in it registers
  this install with publik (a random install id, the app version, macOS and the chip, and your Mac's
  name). It then sends "app opened" and "active session" counts with that install id, and nothing
  about what you listen to. Set `DO_NOT_TRACK=1` or `PUBLIK_TELEMETRY=off` to stop the counts.
  publik's own [privacy policy](https://publikhq.com/privacy) applies. The edition with no account
  and no telemetry is [fjenn/spotless](https://github.com/fjenn/spotless).
- **No other analytics and no crash reporting.** The developer runs no server.
- **Your library, playlists, history, stats and downloads stay on your device.**
- **The app talks to third-party services directly** to work: YouTube for music and videos, and
  lyrics, podcast, audiobook and metadata services (for example LRCLIB, Apple's podcast directory,
  Deezer's public search, MusicBrainz, SponsorBlock, the Internet Archive). They receive what a
  request needs, such as a search term or a song title and artist, plus your IP address, like any
  website would.
- **YouTube sign-in is optional.** The sign-in cookie is kept in the system Keychain on the Mac (in
  the app's own database on Windows and Linux) and is sent only to YouTube/Google. While you are signed in, songs you play are added to your YouTube
  history unless you turn that off in Settings or use a private session.

The full text is on the [privacy page][privacy].

## Code signing policy

Free code signing provided by SignPath.io, certificate by SignPath Foundation.

- **Committers and reviewers:** members of the [fjenn](https://github.com/fjenn) organization.
- **Approvers:** [fjenn](https://github.com/fjenn) (owner).
- **Builds:** Windows binaries are built only from this repository's source code, by GitHub
  Actions (`.github/workflows/release-windows.yml`). Nothing is built or signed on a personal
  machine.
- **Privacy:** the app sends no telemetry. Its network calls go only to the music, podcast and
  lyrics sources it plays from, GitHub (for updates) and, if you link devices, the optional sync
  relay. See [Privacy](#privacy).

## Build from source

### Mac

You need macOS with Xcode's command line tools, [Rust](https://rustup.rs) (stable),
Node.js 20 or later with [pnpm](https://pnpm.io), and libmpv from Homebrew for development.

```bash
brew install mpv
cargo install tauri-cli --locked          # or use ui/node_modules/.bin/tauri after pnpm install
(cd ui && pnpm install)
export LIBRARY_PATH="$(brew --prefix)/lib:$LIBRARY_PATH"
cargo tauri dev                           # run with hot reload
scripts/build-mac.sh --release            # a Spotless.app that uses Homebrew's libmpv
```

For a self-contained app and DMG that runs on other Macs, use `scripts/release-mac.sh`. It bundles
libmpv and FFmpeg from a pinned, signature-checked IINA release, builds a universal binary by
default, and signs ad hoc unless you set `SIGN_IDENTITY` (and `NOTARY_PROFILE` to notarize). See
the comments at the top of the script and [docs/BUILD-PLATFORMS.md](docs/BUILD-PLATFORMS.md).

### iPhone

You need a recent Xcode, [XcodeGen](https://github.com/yonaskolb/XcodeGen) and the iOS Rust
targets.

```bash
brew install xcodegen
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
scripts/build-ios-core.sh                 # the shared core as an xcframework (--release for speed)
cd ios/Encore && xcodegen generate && open Encore.xcodeproj
```

In Xcode, pick your own team under Signing & Capabilities, then run the `Encore` scheme (the
project's internal name; the app is called Spotless). Tests: `SIM=<simulator udid> ios/Encore/scripts/test.sh`.

### Tests

```bash
export LIBRARY_PATH="$(brew --prefix)/lib:$LIBRARY_PATH"
cargo test -p limusic-app --lib
cd ui && pnpm check
```

More in [CONTRIBUTING.md](CONTRIBUTING.md).

## FAQ

<details>
<summary><b>Is it really free?</b></summary>
<br>
Yes. There is no paid tier, no subscription and nothing to unlock. Spotless is open-source
software under the GPL, and it has no servers to pay for: the app runs on your device and fetches
what you play directly from the source.
</details>

<details>
<summary><b>Do I need an account?</b></summary>
<br>
In this publik edition, yes: it opens with Sign in with publik, and sends usage counts to publik.
Your library, playlists and history still live on your device; there is no Spotless server. If you
want no account at all, use [fjenn/spotless](https://github.com/fjenn/spotless). Apart from that,
the only sign-in is the optional one to YouTube, for bringing in your existing YouTube Music library.
</details>

<details>
<summary><b>Is it legal?</b></summary>
<br>
Spotless is an independent open-source app. It does not host or redistribute any music; it plays
what YouTube and the other services serve to any other client. It is not affiliated with,
endorsed by or connected to Spotify, YouTube, Google or any other company whose services it talks
to. Whether and how you use it is up to you, in line with the laws and terms that apply where you
live.
</details>

<details>
<summary><b>Does it work on Windows, Linux or Android?</b></summary>
<br>
Linux: yes, see <a href="#install-on-linux">Install on Linux</a>. Windows is being tested and comes
soon. Android is not planned yet.
</details>

## Credits

Spotless is a fork of [**Limusic**](https://github.com/SimoHypers/limusic) by SimoHypers and
contributors (GPL-3.0), which in turn started from the playback engine of
[Metrolist](https://github.com/mostafaalagamy/Metrolist). Thank you.

It also builds on:

- [Tauri](https://tauri.app) and [Svelte](https://svelte.dev) for the Mac app
- [mpv](https://mpv.io) / libmpv and [FFmpeg](https://ffmpeg.org) for playback; the Mac release
  bundles the libraries from [IINA](https://iina.io), and the iPhone app uses
  [MPVKit](https://github.com/mpvkit/MPVKit)
- [RustyPipe](https://codeberg.org/ThetaDev/rustypipe) and its BotGuard helper
- [UniFFI](https://github.com/mozilla/uniffi-rs) for the Rust core on iOS
- [LRCLIB](https://lrclib.net) and other lyrics providers, [SponsorBlock](https://sponsor.ajay.app),
  [LibriVox](https://librivox.org) and the [Internet Archive](https://archive.org)
- [pypinyin](https://github.com/mozillazg/python-pinyin) data for Mandarin romanization
- [Figtree](https://github.com/erikdkennedy/figtree) for the type

Third-party licences are listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## License

Spotless is free software under the [GNU General Public License v3.0 or later](LICENSE).
All product names and trademarks belong to their owners.

<!-- Links used above, kept in one place. -->
[website]: https://opena.tech/spotless
[privacy]: https://opena.tech/spotless/privacy
[support]: https://opena.tech/spotless/support
[releases]: https://github.com/fjenn/spotless-publik/releases
[bug]: https://github.com/fjenn/spotless/issues/new/choose
