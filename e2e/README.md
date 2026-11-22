# End-to-end tests (Windows, Linux)

The installed app, driven like a user, on GitHub's Windows and Ubuntu machines. Nothing here is
built locally.

- `specs/parity.e2e.mjs`: every feature the Mac app has: search, playback, queue, lyrics,
  library, pages, podcasts, audio, downloads, settings, sign-in, device linking, shortcuts,
  mini player, OS media keys (SMTC / MPRIS), deep links, the tray, the title bar. Screenshots of
  every page land in the run's artifacts, with a pass/fail table in the run summary.
- `specs/update.e2e.mjs` and `scripts/update-windows.ps1`: the in-app updater, from one release
  to a newer one (`.github/workflows/e2e-update.yml`).

How it connects: on Linux, `tauri-driver` with WebKitWebDriver. On Windows, the app is started
with WebView2's DevTools port (`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`) and msedgedriver attaches
to it.

Run it: Actions › E2E (Windows, Linux) › Run workflow, with a release tag. Or pass the run ids of a
Release Windows run and a Release Linux dry run to test unreleased builds.

YouTube refuses anonymous streams to GitHub's datacenter IPs ("Sign in to confirm you're not a
bot"). So on a runner, playback is tested with local files and an RSS podcast. Everything after
the stream URL is the same code: mpv, the sound card, EQ, speed, the queue, media keys. Searching,
browsing, lyrics, radio and Spotify import use the live services.
