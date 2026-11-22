// Spotless feature parity, end to end, on the INSTALLED app (Windows / Linux).
// Every test drives the real UI where a user would, and checks the result through the app's own
// backend (window.__TAURI_INTERNALS__.invoke — the same calls the UI makes) and, where the OS is
// involved, through the OS itself (lib/os.mjs). A screenshot of every page and state lands in
// $E2E_OUT/shots.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import assert from 'node:assert/strict';
import * as sys from '../lib/os.mjs';

const OUT = path.resolve(process.env.E2E_OUT || 'out');
const SHOTS = path.join(OUT, 'shots');
let n = 0;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── helpers ────────────────────────────────────────────────────────────────────────────────────
async function inv(cmd, args = {}) {
	const r = await browser.executeAsync(
		(c, a, done) => {
			window.__TAURI_INTERNALS__.invoke(c, a).then(
				(ok) => done({ ok: ok === undefined ? null : ok }),
				(err) => done({ err: String(err && err.message ? err.message : err) })
			);
		},
		cmd,
		args
	);
	if (r && 'err' in r) throw new Error(`${cmd}: ${r.err}`);
	return r?.ok;
}

async function shot(name) {
	n += 1;
	const file = path.join(SHOTS, `${String(n).padStart(2, '0')}-${name}.png`);
	try {
		await browser.saveScreenshot(file);
	} catch (e) {
		console.warn('screenshot failed', name, e.message);
	}
}

function note(title, text) {
	fs.appendFileSync(path.join(OUT, 'notes.jsonl'), JSON.stringify({ title, note: text }) + '\n');
}

async function until(fn, { timeout = 30_000, every = 500, msg = 'condition' } = {}) {
	const end = Date.now() + timeout;
	let last;
	while (Date.now() < end) {
		try {
			last = await fn();
			if (last) return last;
		} catch (e) {
			last = e;
		}
		await sleep(every);
	}
	throw new Error(`timed out waiting for ${msg}${last instanceof Error ? `: ${last.message}` : ''}`);
}

/** The main window's webview (other webviews: the hidden cipher one, login, mini). */
async function toMain() {
	const handles = await browser.getWindowHandles();
	for (const h of handles) {
		await browser.switchToWindow(h);
		const ok = await browser
			.execute(() => !!window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.metadata?.currentWebview?.label === 'main')
			.catch(() => false);
		if (ok) return h;
	}
	throw new Error(`main webview not found among ${handles.length} handles`);
}

const btn = (label) => $(`[aria-label="${label}"]`);
const byText = (tag, text) => $(`//${tag}[normalize-space(.)="${text}"]`);
const containsText = (tag, text) => $(`//${tag}[contains(normalize-space(.), "${text}")]`);

const fallbacks = [];
async function click(el) {
	await el.waitForExist({ timeout: 20_000 });
	await el.scrollIntoView({ block: 'center' }).catch(() => {});
	await el.waitForDisplayed({ timeout: 20_000 }).catch(() => {});
	try {
		await el.click();
	} catch (e) {
		// Covered for a moment (a tooltip, a toast) or WebDriver's hit test disagreeing with the
		// layout: press it the way the page's own handler sees a click.
		fallbacks.push(String(e.message).slice(0, 80));
		await browser.execute((x) => x.click(), el);
	}
}
async function dbl(el) {
	await el.waitForExist({ timeout: 20_000 });
	await el.scrollIntoView({ block: 'center' }).catch(() => {});
	try {
		await el.doubleClick();
	} catch (e) {
		fallbacks.push(String(e.message).slice(0, 80));
		await browser.execute((x) => x.dispatchEvent(new MouseEvent('dblclick', { bubbles: true, cancelable: true, detail: 2 })), el);
	}
}

/** Client-side navigation, the way a link click does it. */
async function go(href) {
	await browser.execute((h) => {
		const a = document.createElement('a');
		a.href = h;
		document.body.appendChild(a);
		a.click();
		a.remove();
	}, href);
	await until(async () => (await browser.execute(() => location.pathname + location.search)).startsWith(href.split('#')[0]), {
		msg: `navigation to ${href}`
	});
	await sleep(1500);
}

async function dismissDialogs() {
	for (let i = 0; i < 6; i++) {
		const b = await byText('button', 'Not now');
		if (await b.isDisplayed().catch(() => false)) {
			await b.click();
			await sleep(800);
			continue;
		}
		break;
	}
	await browser.keys('Escape');
}

async function playback() {
	return inv('get_playback');
}
async function clock() {
	return inv('playback_clock');
}
/** The position, read twice `ms` apart: proof the audio clock is really moving. */
async function advances(ms = 4000) {
	const a = await clock();
	await sleep(ms);
	const b = await clock();
	return { a: a?.position ?? -1, b: b?.position ?? -1, moved: (b?.position ?? 0) - (a?.position ?? 0) };
}
async function waitPlaying(videoId, timeout = 60_000) {
	return until(
		async () => {
			const p = await playback();
			return p?.now && (!videoId || p.now.videoId === videoId) && !p.now.loading && !p.paused && p.position > 0.5 ? p : null;
		},
		{ timeout, msg: `playback of ${videoId ?? 'a track'}` }
	);
}
async function search(q) {
	const box = await btn('Songs, artists or podcasts');
	await click(box);
	await box.setValue(q);
	await browser.keys('Enter');
	await until(async () => (await browser.execute(() => location.pathname)) === '/search', { msg: 'search page' });
	// The All view is Spotify's mixed layout; the Songs chip is the track table.
	await click(await $('//button[normalize-space(.)="Songs"]'));
	await $('[data-track-row]').waitForDisplayed({ timeout: 45_000 });
	await sleep(1000);
}

const SONG_Q = 'Blinding Lights The Weeknd';
const state = {};

// ── the suite ──────────────────────────────────────────────────────────────────────────────────
describe('Startup and window', () => {
	it('starts and shows the main window', async () => {
		await until(async () => (await browser.getWindowHandles()).length > 0, { timeout: 90_000, msg: 'a window' });
		await until(() => toMain().then(() => true), { timeout: 60_000, msg: 'main webview' });
		await $('[aria-label="Now playing bar"]').waitForDisplayed({ timeout: 60_000 });
		state.version = await browser.execute(() => window.__TAURI_INTERNALS__.invoke('plugin:app|version'));
		await shot('first-launch');
		const vis = await inv('plugin:window|is_visible', { label: 'main' });
		assert.equal(vis, true, 'main window visible');
		note('starts and shows the main window', `version ${state.version}`);
	});

	it('first-run Spotify import offer, then dismiss', async () => {
		const offer = await containsText('*', 'Spotify');
		await offer.waitForDisplayed({ timeout: 20_000 }).catch(() => {});
		await shot('welcome-import-offer');
		await dismissDialogs();
		await sleep(1000);
		await dismissDialogs();
		await shot('home');
		assert.equal(await $('[role="dialog"]').isDisplayed().catch(() => false), false, 'no dialog left open');
	});

	it('home page loads shelves', async () => {
		await until(async () => (await $$('main a[href]')).length > 5, { timeout: 60_000, msg: 'home content' });
		await shot('home-loaded');
	});

	it('window controls: maximize, restore, minimize, unminimize', async () => {
		assert.ok(await btn('Minimize').isDisplayed(), 'Minimize button');
		assert.ok(await btn('Close').isDisplayed(), 'Close button');
		await click(await btn('Maximize'));
		await until(() => inv('plugin:window|is_maximized', { label: 'main' }), { msg: 'maximized' });
		await shot('maximized');
		await click(await btn('Restore'));
		await until(async () => !(await inv('plugin:window|is_maximized', { label: 'main' })), { msg: 'restored' });
		await click(await btn('Minimize'));
		await until(() => inv('plugin:window|is_minimized', { label: 'main' }), { msg: 'minimized' });
		await inv('show_main');
		await until(async () => !(await inv('plugin:window|is_minimized', { label: 'main' })), { msg: 'unminimized' });
	});

});

describe('Search', () => {
	it('typeahead suggestions while typing', async () => {
		const box = await btn('Songs, artists or podcasts');
		await click(box);
		await box.setValue('daft pu');
		const list = await $('[data-testid="search-suggestion"], [data-testid="search-recent"]');
		await list.waitForDisplayed({ timeout: 30_000 });
		await shot('search-typeahead');
		await browser.keys('Escape');
	});

	it('songs, artists, albums, playlists', async () => {
		const r = await inv('search_all', { query: SONG_Q, recordHistory: false });
		state.artist = r.artists[0];
		state.album = r.albums[0];
		state.playlist = r.playlists[0];
		state.songs = r.songs;
		const box = await btn('Songs, artists or podcasts');
		await click(box);
		await box.setValue(SONG_Q);
		await browser.keys('Enter');
		await until(async () => (await browser.execute((t) => (document.querySelector('main')?.innerText ?? '').includes(t), r.songs[0].title)), { timeout: 45_000, msg: 'All results' });
		await sleep(1500);
		await shot('search-all');
		await search(SONG_Q);
		await shot('search-songs');
		assert.ok(r.songs.length > 0, 'songs');
		assert.ok(r.artists.length > 0, 'artists');
		assert.ok(r.albums.length > 0, 'albums');
		assert.ok(r.playlists.length > 0, 'playlists');
		state.artist = r.artists[0];
		state.album = r.albums[0];
		state.playlist = r.playlists[0];
		state.songs = r.songs;
	});

	it('podcasts', async () => {
		const r = await inv('podcast_search', { query: 'The Daily New York Times', scope: null });
		assert.ok(r.shows.length > 0, 'podcast shows');
		state.show = r.shows[0];
		const chip = await $('//button[normalize-space(.)="Podcasts & Shows" or normalize-space(.)="Podcasts"]');
		if (await chip.isDisplayed().catch(() => false)) {
			await chip.click();
			await sleep(3000);
			await shot('search-podcasts-chip');
			await browser.back().catch(() => {});
		}
	});
});

function toSong(b) {
	return { video_id: b.id, title: b.title, artists: b.subtitle ?? '', thumbnail: b.thumbnail ?? undefined };
}

// YouTube answers GitHub's datacenter IPs with "Sign in to confirm you're not a bot" (ip-ban) for
// every anonymous stream, so on a runner the playback tests play what doesn't come from YouTube:
// local files (mpv → the sound card) and an RSS podcast (mpv streaming https itself). Everything
// downstream of the stream URL — mpv, the audio device, EQ, speed, queue, media keys — is the same
// code either way. Searching, browsing, lyrics and radio still use YouTube Music / LRCLIB live.
const LOCAL_DIR = path.join(os.tmpdir(), 'spotless e2e music');
const LOCAL_FILES = [
	['E2E Artist - Tone A.wav', 90, 440],
	['E2E Artist - Tone B.wav', 90, 550],
	['E2E Artist - Tone C.wav', 90, 660],
	['The Weeknd - Blinding Lights.wav', 200, 330]
];
const RSS_FEED = 'https://feeds.npr.org/510318/podcast.xml'; // NPR Up First: short daily episodes

async function localSongs() {
	return (await inv('get_local_library')).songs;
}
async function playLocal(title) {
	const s = (await localSongs()).find((x) => x.title === title);
	await inv('play', { item: s, from: null });
	return waitPlaying(s.video_id, 30_000);
}
const row = (text) => $(`//*[@data-track-row][contains(normalize-space(.), "${text}")]`);

describe('Spotify import', () => {
	// First, before anything touches YouTube's player: the importer pauses itself for an hour after
	// any bot check the app sees (by design), and the runner's IP draws one on the first stream.
	it('Spotify import from a public playlist link', async () => {
		let preview;
		for (const l of [
			'https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M',
			'https://open.spotify.com/album/4yP0hdKOZPNshxUOjY0cZj',
			'https://open.spotify.com/playlist/37i9dQZEVXbMDoHDwVN2tF'
		]) {
			preview = await inv('import_read', { link: l }).catch((e) => ({ error: e.message }));
			if (preview && !preview.error && preview.lists?.length) break;
		}
		assert.ok(preview?.lists?.length, `import read: ${JSON.stringify(preview).slice(0, 200)}`);
		const snap = await inv('import_start', { lists: [0] });
		const done = await until(
			async () => {
				const s = await inv('import_status');
				return s && (s.done >= Math.min(5, s.total) || /done|review|finished/i.test(s.phase)) ? s : null;
			},
			{ timeout: 180_000, every: 2000, msg: 'import matching' }
		);
		await sleep(1000);
		await shot('import-progress');
		assert.ok(done.matched > 0, `matched ${done.matched}/${done.total}`);
		await inv('import_cancel').catch(() => {});
		note('Spotify import from a public playlist link', `${preview.lists[0].name}: matched ${done.matched} of first ${done.done} (${snap.total} total)`);
		await browser.keys('Escape');
	});
});

describe('Playback', () => {
	it('local folder scanned (files from disk)', async () => {
		for (const [f, secs, hz] of LOCAL_FILES) sys.writeWav(path.join(LOCAL_DIR, f), secs, hz);
		await inv('add_local_folder', { path: LOCAL_DIR });
		const songs = await until(async () => {
			const s = await localSongs();
			return s.length >= LOCAL_FILES.length ? s : null;
		}, { timeout: 30_000, msg: 'scanned files' });
		await go('/collection/local-files');
		await row('Tone A').waitForDisplayed({ timeout: 20_000 });
		await shot('local-files');
		note('local folder scanned (files from disk)', `${LOCAL_DIR}: ${songs.map((s) => `${s.artists} – ${s.title}`).join(', ')}`);
	});

	it('plays a track (double-click), position advances on the sound card', async () => {
		await dbl(await row('Tone A'));
		const p = await waitPlaying(null, 30_000);
		state.first = p.now;
		const m = await advances(5000);
		assert.ok(m.moved > 3.5, `position moved ${m.moved.toFixed(2)}s in 5s (${m.a}→${m.b})`);
		await shot('playing');
		note('plays a track (double-click), position advances on the sound card', `"${p.now.title}" moved ${m.moved.toFixed(1)}s/5s; UI fallbacks: ${fallbacks.length}`);
	});

	it('pause and resume from the player bar', async () => {
		await click(await btn('Pause'));
		await until(async () => (await playback()).paused, { msg: 'paused' });
		const m = await advances(2500);
		assert.ok(Math.abs(m.moved) < 0.3, `position held while paused (${m.moved})`);
		await shot('paused');
		await click(await btn('Play'));
		await until(async () => !(await playback()).paused, { msg: 'resumed' });
		assert.ok((await advances(3000)).moved > 2, 'moves again');
	});

	it('seek by clicking the progress bar', async () => {
		const bar = await btn('Change progress');
		await bar.waitForDisplayed();
		const { width } = await bar.getSize();
		const dur = (await playback()).duration;
		// WebDriver offsets are from the element's centre: +25% of the width lands at 75%.
		await bar.click({ x: Math.round(width * 0.25) });
		await until(async () => Math.abs((await clock()).position - dur * 0.75) < dur * 0.08, { msg: `seek near 75% of ${dur}s` });
		await inv('seek', { position: 20 });
		await until(async () => Math.abs((await clock()).position - 20) < 2, { msg: 'seek to 20s' });
		assert.ok((await advances(3000)).moved > 2, 'plays on after seek');
	});

	it('volume: mute, unmute, set', async () => {
		await click(await btn('Mute'));
		await until(async () => (await playback()).volume === 0, { msg: 'muted' });
		await click(await btn('Unmute'));
		await until(async () => (await playback()).volume > 0, { msg: 'unmuted' });
		// Drag target: a click at 40% of the volume slider.
		const vol = await btn('Change volume');
		if (!(await vol.isDisplayed())) {
			// Below 1000 px the bar hides the slider (as Spotify's does); the keyboard still works.
			const v0 = Number(await browser.execute(() => document.querySelector('[aria-label="Change volume"]')?.getAttribute('aria-valuenow')));
			await browser.execute(() => document.activeElement?.blur?.());
			await browser.keys(['Control', 'ArrowDown']);
			const v1 = await until(async () => {
				const v = Number(await browser.execute(() => document.querySelector('[aria-label="Change volume"]')?.getAttribute('aria-valuenow')));
				return v < v0 ? v : null;
			}, { msg: 'Ctrl+Down lowered the volume' });
			assert.ok((await advances(2000)).moved > 1, 'still playing');
			note('volume: mute, unmute, set', `narrow window: Ctrl+Down ${v0} → ${v1}`);
			return;
		}
		const { width } = await vol.getSize();
		// A real pointer press (WebDriver's element click refuses when a tooltip overlaps it).
		await browser
			.action('pointer', { parameters: { pointerType: 'mouse' } })
			.move({ origin: vol, x: Math.round(width * (0.4 - 0.5)), y: 0 })
			.down({ button: 0 })
			.up({ button: 0 })
			.perform();
		const now = await until(async () => {
			const v = Number(await vol.getAttribute('aria-valuenow'));
			return Math.abs(v - 40) <= 6 ? v : null;
		}, { msg: 'volume slider at ~40' });
		assert.ok((await advances(2000)).moved > 1, 'still playing at the new volume');
		note('volume: mute, unmute, set', `slider ${now}`);
	});

	it('next and previous', async () => {
		const first = (await playback()).now.videoId;
		await click(await btn('Next'));
		const p = await until(async () => {
			const s = await playback();
			return s.now && s.now.videoId !== first && s.position > 0.5 && !s.paused ? s : null;
		}, { timeout: 30_000, msg: 'next track playing' });
		assert.ok((await advances(3000)).moved > 2, 'next track plays');
		await click(await btn('Previous'));
		await sleep(400);
		if ((await playback()).now?.videoId !== first) await click(await btn('Previous'));
		await until(async () => (await playback()).now?.videoId === first, { timeout: 30_000, msg: 'back to the first track' });
		note('next and previous', `next was "${p.now.title}"`);
	});

	it('shuffle and repeat', async () => {
		const q0 = await inv('get_queue');
		await click(await $('[aria-label*="huffle"]'));
		await until(async () => (await inv('get_queue')).shuffle !== q0.shuffle, { msg: 'shuffle toggled' });
		await click(await btn('Enable repeat'));
		await until(async () => (await inv('get_queue')).repeat === 'all', { msg: 'repeat all' });
		await click(await btn('Enable repeat one'));
		await until(async () => (await inv('get_queue')).repeat === 'one', { msg: 'repeat one' });
		await click(await btn('Disable repeat'));
		await until(async () => (await inv('get_queue')).repeat === 'off', { msg: 'repeat off' });
		await shot('shuffle-repeat');
		if ((await inv('get_queue')).shuffle) await inv('toggle_shuffle');
	});
});

describe('Queue, radio and lyrics', () => {
	it('add to queue from the track menu', async () => {
		await go('/collection/local-files');
		await (await row('Tone C')).click({ button: 'right' });
		await click(await $('//*[@role="menuitem"][contains(normalize-space(.), "Add to queue")]'));
		await sleep(1000);
		const q = await inv('get_queue');
		const upcoming = q.items.slice(q.currentIndex + 1).map((s) => s.title);
		assert.ok(upcoming.includes('Tone C'), `Tone C queued: ${upcoming.join(' | ')}`);
	});

	it('queue panel shows it; reorder and remove', async () => {
		await click(await btn('Queue'));
		await sleep(1500);
		await shot('queue-panel');
		assert.ok(await browser.execute(() => document.body.innerText.includes('Tone C')), 'queued row on screen');
		const q = await inv('get_queue');
		const lastIdx = q.items.length - 1;
		const lastId = q.items[lastIdx].video_id;
		await inv('move_in_queue', { from: lastIdx, to: q.currentIndex + 1 });
		const q2 = await inv('get_queue');
		assert.equal(q2.items[q2.currentIndex + 1].video_id, lastId, 'moved to next up');
		await inv('remove_from_queue', { index: q2.currentIndex + 1 });
		const q3 = await inv('get_queue');
		assert.equal(q3.items.length, q2.items.length - 1, 'removed');
		await click(await btn('Queue'));
	});

	it('lyrics view: synced lines for the playing track', async () => {
		const p = await playLocal('Blinding Lights');
		await inv('seek', { position: 60 });
		const ly = await until(() => inv('get_lyrics', { videoId: p.now.videoId, title: p.now.title, artists: p.now.artists, duration: 200 }), { timeout: 30_000, msg: 'lyrics' });
		assert.ok(ly.lines.length > 5, 'lyrics found');
		await click(await btn('Lyrics'));
		const line = ly.lines.find((l) => l.text && l.text.length > 8)?.text;
		await until(async () => await browser.execute((t) => document.body.innerText.includes(t), line), { msg: `lyric "${line}" on screen` });
		await sleep(1500);
		await shot('lyrics');
		await click(await btn('Lyrics'));
		note('lyrics view: synced lines for the playing track', `${ly.lines.length} lines from ${ly.source}, synced ${ly.synced}`);
	});

	it('song radio fills the queue (YouTube Music)', async () => {
		const s = state.songs[0];
		// Queue-building only: the radio's YouTube streams can't play from the runner's IP.
		await inv('start_radio', { kind: 'song', id: s.id, name: s.title, start: null }).catch((e) => note('song radio fills the queue (YouTube Music)', e.message));
		const q = await until(async () => {
			const x = await inv('get_queue');
			return x.items.length > 10 ? x : null;
		}, { timeout: 120_000, every: 2000, msg: 'radio tracks' });
		await shot('radio-queue');
		note('song radio fills the queue (YouTube Music)', `${q.items.length} tracks from "${q.sourceName}"`);
		await playLocal('Tone A');
	});
});

describe('Library', () => {
	it('like a song from its menu → Liked Songs', async () => {
		await search(SONG_Q);
		await (await $('[data-track-row]')).click({ button: 'right' });
		await click(await $('//*[@role="menuitem"][contains(normalize-space(.), "Save to your Liked Songs")]'));
		const id = state.songs[0].id;
		await until(async () => (await inv('lib_liked_ids')).length > 0, { msg: 'liked' });
		const libText = () => browser.execute(() => document.querySelector('[aria-label="Your Library"]')?.closest('aside, nav, section, div')?.innerText ?? document.body.innerText);
		await until(async () => (await libText()).includes('Liked Songs'), { msg: 'Liked Songs in the sidebar' });
		await go('/collection/tracks').catch(() => {});
		await click(await $('//aside//*[normalize-space(text())="Liked Songs"] | //*[@aria-label="Your Library"]//*[normalize-space(text())="Liked Songs"]'));
		await sleep(2500);
		await shot('liked-songs');
		assert.ok((await $$('[data-track-row]')).length >= 1, 'row on the Liked Songs page');
		note('like a song from its menu → Liked Songs', `liked ${(await inv('lib_liked_ids')).join(', ')} (searched ${id})`);
	});

	it('playlist: create (sidebar), rename, add, remove, delete', async () => {
		const before = (await inv('lib_playlists')).length;
		await click(await $('[aria-label="Create"]'));
		await click(await $('//*[@role="menuitem"][contains(normalize-space(.), "Playlist")]'));
		const pl = await until(async () => {
			const l = await inv('lib_playlists');
			return l.length > before ? l.at(-1) : null;
		}, { msg: 'playlist created' });
		await sleep(1500);
		await shot('playlist-new');
		await browser.keys('Escape');
		const renamed = await inv('lib_playlist_update', { id: pl.id, name: 'E2E Mix', description: 'made by the test' });
		assert.equal(renamed.name, 'E2E Mix');
		const sidebarItem = () => $('//*[normalize-space(text())="E2E Mix"]');
		await (await sidebarItem()).waitForDisplayed({ timeout: 15_000 });
		const songs = [...(await localSongs()).slice(0, 2), ...state.songs.slice(0, 1).map(toSong)];
		await inv('lib_playlist_add', { id: pl.id, songs, allowDuplicates: false, position: null });
		let entries = await inv('lib_playlist_entries', { id: pl.id });
		assert.equal(entries.length, songs.length, 'songs added');
		await click(await sidebarItem());
		await until(async () => (await $$('[data-track-row]')).length === songs.length, { msg: 'rows on the playlist page' });
		await shot('playlist-page-local');
		const rowId = entries[0].rowId ?? entries[0].row_id ?? entries[0].id;
		await inv('lib_playlist_remove', { id: pl.id, rowIds: [rowId] });
		entries = await inv('lib_playlist_entries', { id: pl.id });
		assert.equal(entries.length, songs.length - 1, 'one removed');
		state.localPlaylist = pl.id;
		const tmp = await inv('lib_playlist_create', { name: 'E2E Delete me', description: null, folderId: null });
		await inv('lib_playlist_delete', { id: tmp.id });
		assert.ok(!(await inv('lib_playlists')).some((p) => p.id === tmp.id), 'deleted');
	});

	it('folders and pins in the sidebar', async () => {
		const f = await inv('lib_folder_create', { name: 'E2E Folder', parentId: null });
		await $('//*[normalize-space(text())="E2E Folder"]').waitForDisplayed({ timeout: 15_000 });
		await inv('lib_pin', { itemId: state.localPlaylist, kind: 'playlist', index: null });
		assert.ok((await inv('lib_pins')).some((p) => JSON.stringify(p).includes(state.localPlaylist)), 'pinned');
		await sleep(1000);
		await shot('sidebar-folder-pin');
		note('folders and pins in the sidebar', `folder ${f.id}`);
	});

	it('sidebar filters and sort', async () => {
		const chip = await $('//button[normalize-space(.)="Playlists"]');
		if (await chip.isDisplayed().catch(() => false)) {
			await click(chip);
			await sleep(800);
			await shot('sidebar-filter-playlists');
			await click(chip);
		}
		await click(await $('[aria-label*=" view"]'));
		await click(await $('//*[@role="menuitemcheckbox" or @role="menuitemradio" or @role="menuitem"][contains(normalize-space(.), "Alphabetical")]'));
		await sleep(800);
		await shot('sidebar-sorted');
		assert.ok((await (await $('[aria-label*=" view"]')).getAttribute('aria-label')).startsWith('Alphabetical'), 'sort applied');
	});

	it('private session, explicit filter and blocklist', async () => {
		await inv('lib_set_private_session', { enabled: true });
		assert.ok(JSON.stringify(await inv('lib_private_session')).includes('true'), 'private on');
		await sleep(800);
		await shot('private-session');
		await inv('lib_set_private_session', { enabled: false });
		await inv('smart_set_setting', { key: 'explicit_filter', value: 'true' });
		assert.equal((await inv('smart_settings')).explicit_filter, true);
		await inv('smart_set_setting', { key: 'explicit_filter', value: 'false' });
		const a = state.artist;
		const blocked = await inv('block_artist', { id: a.id, name: a.title });
		assert.ok(blocked.some((b) => JSON.stringify(b).includes(a.id) || JSON.stringify(b).includes(a.title)), 'blocked');
		const key = blocked.find((b) => JSON.stringify(b).includes(a.id))?.key ?? a.id;
		const after = await inv('unblock_artist', { key });
		assert.ok(!after.some((b) => JSON.stringify(b).includes(a.id)), 'unblocked');
	});
});

describe('Pages', () => {
	it('artist page and follow', async () => {
		await go(`/artist/${state.artist.id}`);
		await until(async () => await browser.execute((t) => document.body.innerText.includes(t), state.artist.title), { msg: 'artist name' });
		await sleep(2000);
		await shot('artist');
		await click(await $('//button[normalize-space(.)="Follow"]'));
		await $('//button[normalize-space(.)="Following"]').waitForDisplayed({ timeout: 15_000 });
		const saved = await inv('lib_saved', { kind: null });
		assert.ok(saved.some((s) => JSON.stringify(s).includes(state.artist.id)), 'artist in the library');
	});

	it('album page, save, play queues the album', async () => {
		await go(`/album/${state.album.id}`);
		await $('[data-track-row]').waitForDisplayed({ timeout: 45_000 });
		await shot('album');
		await click(await $('[aria-label="Save to Your Library"]'));
		await until(async () => (await inv('lib_saved', { kind: null })).some((s) => JSON.stringify(s).includes(state.album.id)), { msg: 'album saved' });
		const rows = (await $$('[data-track-row]')).length;
		await click(await $('main [aria-label^="Play"]'));
		const q = await until(async () => {
			const x = await inv('get_queue');
			return x.items.length >= rows && x.sourceName ? x : null;
		}, { timeout: 30_000, msg: 'album queued' });
		note('album page, save, play queues the album', `${q.items.length} tracks from "${q.sourceName}" (YouTube streams: not playable from the runner's IP)`);
		await playLocal('Tone A');
	});

	it('YouTube Music playlist page', async () => {
		await go(`/playlist/${state.playlist.id}`);
		await $('[data-track-row]').waitForDisplayed({ timeout: 45_000 });
		await shot('playlist-yt');
	});

	it('podcast (RSS): show, follow, episode plays, resume', async () => {
		const show = await inv('podcast_add_feed', { url: RSS_FEED });
		state.rss = show;
		await go(`/show/${encodeURIComponent(show.id)}`);
		await until(async () => await browser.execute((t) => document.body.innerText.includes(t), show.title), { timeout: 45_000, msg: 'show title' });
		await sleep(2000);
		await shot('podcast-show');
		await until(async () => (await inv('podcast_follows')).some((s) => s.id === show.id), { timeout: 10_000, msg: 'show followed' }).catch(async () => {
			await click(await $('//button[normalize-space(.)="Follow"]'));
			await until(async () => (await inv('podcast_follows')).some((s) => s.id === show.id), { msg: 'show followed' });
		});
		const page = await inv('podcast_show', { id: show.id, sort: null });
		const ep = page.episodes.find((e) => (e.durationSecs ?? 600) > 300) ?? page.episodes[0];
		state.ep = ep;
		await go(`/episode/${encodeURIComponent(ep.id)}`);
		await sleep(2500);
		await shot('podcast-episode');
		await click(await $('main [aria-label^="Play"]'));
		await until(async () => {
			const s = await playback();
			return s.now?.videoId === ep.id && s.position > 0.5 && !s.paused;
		}, { timeout: 60_000, msg: 'episode playing' });
		assert.ok((await advances(4000)).moved > 3, 'episode streams');
		await shot('podcast-playing');
		await inv('seek', { position: 120 });
		await sleep(6000);
		await click(await btn('Pause'));
		await sleep(2000);
		const prog = await until(async () => {
			const r = await inv('podcast_progress', { ids: [ep.id] });
			return r[ep.id] && r[ep.id].positionSecs > 100 ? r[ep.id] : null;
		}, { timeout: 30_000, msg: 'episode progress saved' });
		await playLocal('Tone B');
		await go(`/episode/${encodeURIComponent(ep.id)}`);
		await click(await $('main [aria-label^="Play"]'));
		const resumed = await until(async () => {
			const s = await playback();
			return s.now?.videoId === ep.id && s.position > 1 ? s.position : null;
		}, { timeout: 60_000, msg: 'episode playing again' });
		assert.ok(resumed > 100, `resumed at ${resumed.toFixed(0)}s (saved ${prog.positionSecs.toFixed(0)}s)`);
		note('podcast (RSS): show, follow, episode plays, resume', `${show.title}: "${ep.title}" resumed at ${resumed.toFixed(0)}s`);
	});

	it('podcast (YouTube): show page from search', async () => {
		const r = await inv('podcast_search', { query: 'The Daily New York Times', scope: null });
		assert.ok(r.shows.length > 0, 'podcast shows');
		await go(`/show/${encodeURIComponent(r.shows[0].id)}`);
		await until(async () => await browser.execute((t) => document.body.innerText.includes(t), r.shows[0].title), { timeout: 45_000, msg: 'show title' });
		await sleep(1500);
		await shot('podcast-show-youtube');
	});

	it('browse, charts, new releases, history, stats pages', async () => {
		for (const [href, name] of [
			['/browse/podcasts', 'browse-podcasts'],
			['/charts', 'charts'],
			['/new-releases', 'new-releases'],
			['/history', 'history'],
			['/stats', 'stats'],
			['/collection/episodes', 'your-episodes']
		]) {
			await go(href);
			await sleep(3500);
			const err = await browser.execute(() => /Something went wrong|Couldn.t load/i.test(document.querySelector('main')?.innerText ?? ''));
			await shot(name);
			assert.equal(err, false, `${href} shows an error`);
		}
		const stats = await inv('listening_stats', { since: 0 });
		assert.ok(stats.secs > 0, `stats recorded (${stats.plays} plays, ${stats.secs}s)`);
		note('browse, charts, new releases, history, stats pages', `${stats.plays} plays, ${stats.secs}s listened`);
	});
});

describe('Audio', () => {
	it('equalizer presets and custom gains', async () => {
		await playLocal('Tone A');
		const info = await inv('audio_eq_info');
		const presets = (info.presets ?? []).map((p) => p.name ?? p);
		const name = presets.find((p) => /bass/i.test(p)) ?? presets[1] ?? presets[0];
		const eq = await inv('set_eq_preset', { name });
		const gains = eq.gains.map((_, i) => (i === 0 ? 6 : 0));
		const eq2 = await inv('set_eq', { enabled: true, gains, commit: true });
		assert.equal(eq2.gains[0], 6);
		assert.ok((await advances(3000)).moved > 2, 'still playing with the EQ on');
		await inv('set_eq', { enabled: false, gains, commit: true });
		note('equalizer presets and custom gains', `preset "${name}" of ${presets.length}`);
	});

	it('crossfade, speed, volume level', async () => {
		assert.equal(await inv('set_crossfade_secs', { secs: 5 }), 5);
		assert.equal((await inv('audio_settings')).crossfadeSecs, 5);
		await inv('set_crossfade_secs', { secs: 0 });
		const p = await playback();
		await inv('set_playback_params', { speed: 1.5, semitones: 0, reverb: p.reverb, target: null });
		await sleep(1500);
		const m = await advances(4000);
		assert.ok(m.moved > 5, `1.5x speed: ${m.moved.toFixed(2)}s in 4s`);
		await inv('set_playback_params', { speed: 1, semitones: 0, reverb: p.reverb, target: null });
		note('crossfade, speed, volume level', `1.5x: ${m.moved.toFixed(2)}s per 4s`);
	});

	it('sleep timer', async () => {
		const s = await inv('set_sleep_timer', { minutes: 15, endOfTrack: false });
		assert.equal(s.mode?.kind, 'at');
		assert.ok(s.remaining > 800, 'about 15 min left');
		const e = await inv('set_sleep_timer', { minutes: null, endOfTrack: true });
		assert.equal(e.mode?.kind, 'endOfTrack');
		const c = await inv('cancel_sleep_timer');
		assert.equal(c.mode, null);
	});

	it('output devices listed and selectable', async () => {
		const d = await inv('audio_devices');
		fs.writeFileSync(path.join(OUT, 'audio-devices.json'), JSON.stringify(d, null, 1));
		const real = d.devices.filter((x) => x.name !== 'auto');
		assert.ok(real.length > 0, `devices: ${d.devices.map((x) => x.name).join(', ')}`);
		const pick = real.find((x) => /e2e|speaker|scream/i.test(`${x.name} ${x.description}`)) ?? real[0];
		const before = await playback();
		if (before.paused || !before.now) await playLocal('Tone B');
		const s = await inv('set_audio_device', { name: pick.name });
		assert.equal(s.selected, pick.name);
		await sleep(1500);
		const m = await advances(3000);
		const after = await playback();
		assert.ok(m.moved > 2, `plays on ${pick.name}: moved ${m.moved.toFixed(2)}s; paused ${after.paused}; "${after.now?.title}" at ${after.position.toFixed(1)}/${after.duration}`);
		await inv('set_audio_device', { name: 'auto' });
		await click(await btn('Connect to a device'));
		await sleep(1500);
		await shot('devices-panel');
		await click(await btn('Connect to a device'));
		note('output devices listed and selectable', d.devices.map((x) => x.description ?? x.name).join(' / '));
	});
});

describe('Files', () => {
	it('download a podcast episode for offline, then play it', async () => {
		const ep = state.ep;
		const song = { video_id: ep.id, title: ep.title, artists: ep.showTitle };
		await inv('downloads_add', { collection: { id: ep.id, kind: 'episode', title: ep.title }, songs: [song] });
		const snap = await until(async () => {
			const d = await inv('downloads_snapshot');
			return d.done.includes(ep.id) ? d : null;
		}, { timeout: 240_000, every: 2000, msg: 'download done' });
		const files = fs.readdirSync(snap.dir, { recursive: true }).filter((f) => !String(f).endsWith('.part'));
		assert.ok(files.length > 0, `files in ${snap.dir}`);
		note('download a podcast episode for offline, then play it', `${snap.dir}: ${files.join(', ').slice(0, 200)}`);
		await go('/settings').catch(() => {});
		await shot('downloads');
	});
});

describe('Settings and accounts', () => {
	it('settings page', async () => {
		await go('/settings');
		await sleep(2500);
		await shot('settings-top');
		for (let i = 1; i <= 4; i++) {
			await browser.execute(() => {
				const m = document.querySelector('main') ?? document.scrollingElement;
				const sc = [...document.querySelectorAll('*')].find((e) => e.scrollHeight > e.clientHeight + 200 && getComputedStyle(e).overflowY !== 'visible' && e.closest('main'));
				(sc ?? m).scrollBy(0, 700);
			});
			await sleep(700);
			await shot(`settings-${i}`);
		}
		const set = await inv('get_settings');
		assert.ok(Object.keys(set).length > 0, 'settings readable');
	});

	it('check for updates (Settings › About)', async () => {
		const b = await $('//button[contains(normalize-space(.), "Check for updates")]');
		if (await b.isExisting()) {
			await click(b);
			const r = await $('[data-testid="update-result"]');
			await r.waitForDisplayed({ timeout: 30_000 });
			const text = await r.getText();
			await shot('update-check');
			assert.ok(!/fail|error/i.test(text), `update check: ${text}`);
			note('check for updates (Settings › About)', text);
		} else {
			const ok = await browser.executeAsync((done) =>
				window.__TAURI_INTERNALS__.invoke('plugin:updater|check', {}).then(() => done('ok'), (e) => done(String(e)))
			);
			assert.equal(ok, 'ok');
		}
		note('can self-update', String(await inv('can_self_update')));
	});

	it('YouTube sign-in window opens and cancels', async () => {
		const wins = async () => (await inv('plugin:window|get_all_windows').catch(() => [])) ?? [];
		inv('login_webview', { addAccount: false }).catch(() => {});
		await until(async () => (await wins()).includes('login'), { timeout: 30_000, msg: 'login window' });
		await sleep(4000);
		await inv('cancel_login');
		await until(async () => !(await wins()).includes('login'), { timeout: 15_000, msg: 'login window closed' });
		await sleep(3000);
		// The app is still whole: main answers and plays on.
		await toMain();
		assert.ok((await inv('get_settings')) && (await wins()).includes('main'), 'main window alive');
		const log = sys.logText();
		const after = log.slice(log.lastIndexOf('sign-in window finished'));
		const webviewErrors = after.split('\n').filter((l) => /WebView2 error|ERROR tauri_runtime_wry/.test(l));
		assert.equal(webviewErrors.length, 0, `after closing the sign-in window: ${webviewErrors.slice(0, 2).join(' || ')}`);
		note('YouTube sign-in window opens and cancels', `windows: ${(await wins()).join(', ')}`);
	});

	it('device linking: pairing code from the relay', async () => {
		const p = await inv('sync_start_pairing');
		assert.ok(p.code && p.code.length >= 4, `pairing code ${p.code}`);
		const st = await inv('sync_status');
		assert.ok(st.pairing, 'pairing in progress');
		await inv('sync_cancel_pairing');
		await go('/settings');
		const dev = await $('[data-testid="devices-status"]');
		if (await dev.isExisting()) await dev.scrollIntoView({ block: 'center' });
		await sleep(1000);
		await shot('devices-settings');
		note('device linking: pairing code from the relay', `relay ok ${st.relay.ok}, platform ${st.platform}, LAN port ${st.lanPort}`);
	});
});

describe('Keyboard, mini player, fullscreen', () => {
	it('shortcuts sheet shows Ctrl, not ⌘', async () => {
		await go('/');
		await browser.keys(['Control', '/']);
		await sleep(1500);
		const text = await browser.execute(() => document.body.innerText);
		await shot('shortcuts-sheet');
		await browser.keys('Escape');
		assert.ok(text.includes('Ctrl'), 'Ctrl labels');
		assert.ok(!text.includes('⌘'), 'no ⌘ anywhere on screen');
	});

	it('Space toggles play/pause; Ctrl+K focuses search; Ctrl+Shift+Down mutes', async () => {
		await playLocal('Blinding Lights');
		await browser.execute(() => document.activeElement?.blur?.());
		const p0 = (await playback()).paused;
		await browser.keys(' ');
		await until(async () => (await playback()).paused !== p0, { msg: 'Space toggled' });
		await browser.keys(' ');
		await until(async () => (await playback()).paused === p0, { msg: 'Space toggled back' });
		await browser.keys(['Control', 'k']);
		await sleep(500);
		const focused = await browser.execute(() => document.activeElement?.getAttribute('aria-label'));
		assert.equal(focused, 'Songs, artists or podcasts', 'search focused');
		await browser.keys('Escape');
		await browser.execute(() => document.activeElement?.blur?.());
		const v = (await playback()).volume;
		await browser.keys(['Control', 'Shift', 'ArrowDown']);
		await until(async () => (await playback()).volume === 0, { msg: 'muted by shortcut' });
		await browser.keys(['Control', 'Shift', 'ArrowDown']);
		await until(async () => (await playback()).volume > 0, { msg: 'unmuted by shortcut' });
		note('Space toggles play/pause; Ctrl+K focuses search; Ctrl+Shift+Down mutes', `volume ${v}`);
	});

	it('mini player opens and closes', async () => {
		await click(await btn('Open Miniplayer'));
		await until(async () => (await inv('plugin:window|get_all_windows').catch(() => [])).includes?.('mini') || (await browser.getWindowHandles()).length > 2, { timeout: 20_000, msg: 'mini window' });
		const handles = await browser.getWindowHandles();
		for (const h of handles) {
			await browser.switchToWindow(h);
			const label = await browser.execute(() => window.__TAURI_INTERNALS__?.metadata?.currentWebview?.label).catch(() => null);
			if (label === 'mini') {
				await sleep(1500);
				await shot('mini-player');
				break;
			}
		}
		await toMain();
		await inv('close_mini');
		await inv('show_main');
	});

	it('full screen (theater) view', async () => {
		await toMain();
		await click(await btn('Enter Full screen'));
		await sleep(2500);
		await shot('fullscreen');
		await browser.keys('Escape');
		await sleep(1000);
	});
});

describe('OS integration', () => {
	it('media session (SMTC / MPRIS) shows the track and obeys play/pause', async () => {
		const p = await playLocal('Blinding Lights');
		const s = await until(() => sys.mediaSession(), { timeout: 30_000, msg: 'OS media session' });
		assert.ok(s.title && p.now.title.toLowerCase().includes(s.title.toLowerCase().slice(0, 6)), `OS shows "${s.title}" for "${p.now.title}"`);
		sys.mediaPlayPause();
		await until(async () => (await playback()).paused, { msg: 'paused by the OS media control' });
		await until(() => /paus/i.test(sys.mediaSession()?.status ?? ''), { msg: 'OS status paused' });
		sys.mediaPlayPause();
		await until(async () => !(await playback()).paused, { msg: 'resumed by the OS media control' });
		note('media session (SMTC / MPRIS) shows the track and obeys play/pause', `${s.app}: "${s.title}" / ${s.artist} (${s.status})`);
	});

	it('deep link spotless:// through the OS opens the page', async () => {
		const handler = sys.urlHandler();
		note('deep link spotless:// through the OS opens the page', `handler: ${handler}`);
		assert.ok(handler, 'spotless:// is registered with the OS');
		await go('/');
		sys.openUrl(`spotless://artist/${state.artist.id}`);
		await until(async () => (await browser.execute(() => location.pathname)) === `/artist/${state.artist.id}`, { timeout: 30_000, msg: 'artist page from the link' });
		await sleep(1500);
		await shot('deep-link-artist');
	});

	it('tray icon: present, menu, brings the window back', async () => {
		const t = await until(() => (sys.trayInfo().present ? sys.trayInfo() : null), { timeout: 20_000, msg: 'tray icon' });
		fs.writeFileSync(path.join(OUT, 'tray.json'), JSON.stringify(t, null, 1));
		note('tray icon: present, menu, brings the window back', `${t.names.join(' | ')}`);
		assert.ok(t.names.length > 0, `tray menu read: ${JSON.stringify(t).slice(0, 300)}`);
		// ✕ hides to the tray; the tray brings it back.
		await click(await btn('Close'));
		await until(async () => !(await inv('plugin:window|is_visible', { label: 'main' })), { msg: 'hidden to tray' });
		if (sys.IS_WIN && !t.clickable) {
			// Registered with Explorer but not reachable through UI Automation on this server's
			// taskbar: bring the window back the way a second launch does instead.
			note('tray icon: present, menu, brings the window back', `Explorer has the icon (${t.registered}); window brought back by a second launch`);
			await sys.launchAgain();
		} else if (sys.IS_WIN) sys.trayInvokeWindows();
		else sys.trayClick(t.names.find((x) => /show|open/i.test(x)) ?? 'Show Spotless');
		await until(() => inv('plugin:window|is_visible', { label: 'main' }), { msg: 'window back from the tray' });
		await toMain();
		if (!sys.IS_WIN) {
			const p0 = (await playback()).paused;
			sys.trayClick(t.names.find((x) => /^(play|pause)$/i.test(x)));
			await until(async () => (await playback()).paused !== p0, { msg: 'tray Play/Pause' });
		}
	});


	it('no menu bar strip; title bar double-click maximizes', async () => {
		await toMain();
		await go('/');
		// The page starts at the window's top edge: no native menu strip above the title bar.
		const top = await browser.execute(() => document.querySelector('header[data-window-drag]')?.getBoundingClientRect().top);
		assert.equal(top, 0, 'title bar at y=0');
		const header = await $('header[data-window-drag]');
		const { width } = await header.getSize();
		const x = -Math.round(width * 0.3); // empty bar left of the search box (offset from centre)
		const dbl2 = () =>
			browser
				.action('pointer', { parameters: { pointerType: 'mouse' } })
				.move({ origin: header, x, y: 0 })
				.down({ button: 0 })
				.up({ button: 0 })
				.pause(60)
				.down({ button: 0 })
				.up({ button: 0 })
				.perform();
		const was = await inv('plugin:window|is_maximized', { label: 'main' });
		await dbl2();
		await until(async () => (await inv('plugin:window|is_maximized', { label: 'main' })) !== was, { timeout: 10_000, msg: 'double-click toggled maximize' });
		await shot('titlebar-doubleclick');
		await dbl2();
		await until(async () => (await inv('plugin:window|is_maximized', { label: 'main' })) === was, { timeout: 10_000, msg: 'double-click toggled back' });
	});
	it('no errors in the app log', async () => {
		const log = sys.logText();
		fs.writeFileSync(path.join(OUT, 'encore.log'), log);
		const errors = log
			.split('\n')
			.filter((l) => /\bERROR\b/.test(l))
			.filter((l) => !/ResizeObserver|favicon|net::ERR_ABORTED/i.test(l));
		// YouTube refusing the runner's datacenter IP ("Sign in to confirm you're not a bot") is the
		// environment, not the app; the app's handling of it (message, skip) is the correct one.
		const env = errors.filter((l) => /without an account|not a bot|ip-ban/i.test(l));
		errors.splice(0, errors.length, ...errors.filter((l) => !env.includes(l)));
		note('no errors in the app log', `${log.split('\n').length} lines, ${errors.length} ERROR (+${env.length} YouTube refusing the runner's IP)`);
		fs.writeFileSync(path.join(OUT, 'log-errors.txt'), errors.join('\n'));
		assert.ok(log.length > 0, 'log found');
		assert.equal(errors.length, 0, errors.slice(0, 3).join(' || '));
	});
});
