## Spotless 1.0.4-rc.11 (test build)

- Windows and Linux: the tray menu's "now playing" row follows every song and podcast (it could read "Nothing playing" while something played).

## Spotless 1.0.4-rc.10 (test build)

- Behind a corporate or system HTTP proxy, songs play again: the app's own local stream links no longer go through the proxy (every track used to fail).
- When a song can't be played, the message stays on screen longer (12 seconds), so it isn't missed.
- Windows: no more log noise from a harmless browser warning every few seconds.

## Spotless 1.0.4-rc.9 (test build)

- Offline, or where YouTube is blocked, playback no longer goes through a crash path in its last-resort fallback: Spotless sees YouTube can't be reached and says so.
- Browse all: tile names never split mid-word ("Podcasts", "Audiobooks"); a tile that is too narrow shrinks its title instead.
- Local files can be added to Liked Songs from the player bar and the Now Playing panel, like any other song.
- Browse › Podcasts › Search podcasts opens search for shows with the cursor in the search box.
- Linux: the .deb and .rpm now pull in libEGL, which the app needs to start; the AppImage needs it from the system (every desktop has it; minimal and server installs: `sudo apt install libegl1`). Without desktop-file-utils, the AppImage no longer logs an error on every launch.

## Spotless 1.0.4-rc.8 (test build)

- Windows and Linux: the app says "this PC" / "this computer" and "Show in File Explorer" where it used to say "this Mac" and "Show in Finder".
- Video podcasts on Windows and Linux: if the picture's stream stalls, the sound keeps playing (audio only, with a note).

## Spotless 1.0.4-rc.7 (test build)

- Windows: Settings › Check for updates no longer reports an error while the newest release has no Windows or Linux build; it says you're up to date.
- Linux AppImage: spotless:// links open the app (the AppImage registers itself at launch).
- Clicking between steps on the volume slider no longer fails silently.
- Every feature checked end to end on Windows and Linux by an automated suite (e2e/).

## Spotless 1.0.4-rc.6 (test build)

- New: Spotless for Windows (64-bit, Windows 10 and 11). Same app as on Mac. Not code-signed yet: on first launch Windows may say "Windows protected your PC"; choose More info › Run anyway.
- New: Spotless for Linux (AppImage, .deb, .rpm).

- Video podcasts on Mac: no more stalls, sharper picture, tighter lip sync.
- Podcast and music speeds are separate; podcasts go up to 3x.
- Drag the window by its top bar; back button in full-screen video.
- YouTube Music sign-in signs Spotless in directly.
- Podcast episodes from search play and open correctly.
- Check for updates in Settings and the Spotless menu.
- Link your Mac and iPhone: Settings › Devices, or "Link your iPhone" in the profile menu.
