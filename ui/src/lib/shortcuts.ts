// App keyboard shortcuts, Spotify's map (docs/spotify-spec/screens/keyboard-shortcuts.md).
// Combos the native menu bar owns (core.md: ⌘, ⌘N ⌥⌘N ⌘F ⌘K ⌘←/→ ⌥⇧L ⌥⇧R ⌥⇧Q ⌘=/-/0 ⌥⇧B ⌘/
// ⌥S ⌥R ⌃⌘←/→ ⇧⌘←/→ ⌘↑/↓ ⇧⌘↓ ⌥⌘M) are NOT bound here: the page sees keys first and a
// preventDefault would stop the menu item. Those arrive as `menu://<id>` (shell/menubar.ts).
// In a plain browser (vite dev) there is no native menu, so `BROWSER_FALLBACK` binds them too.
import { browser } from '$app/environment';
import { goto } from '$app/navigation';
import * as api from './api';
import { cycleRepeat, nudgeVolume, playback, setTempoPitch, toggleMute, ui } from './player.svelte';
import { isTauri } from '@tauri-apps/api/core';

export const IS_MAC =
	browser &&
	((navigator as unknown as { userAgentData?: { platform?: string } }).userAgentData?.platform === 'macOS' ||
		/Mac/.test(navigator.userAgent));

export const MOD = IS_MAC ? '⌘' : 'Ctrl+';
/** One key cap as this platform names it: the Mac glyphs, Windows/Linux words (Spotify's own labels). */
const PC_KEYS: Record<string, string> = { '⌘': 'Ctrl', '⌃': 'Ctrl', '⌥': 'Alt', '⇧': 'Shift' };
export const keyCap = (k: string): string => (IS_MAC ? k : (PC_KEYS[k] ?? k));
/** A combo written the Mac way ('⇧⌘L', '⌘,') as this platform shows it ('Ctrl+Shift+L'). */
export function combo(mac: string): string {
	if (IS_MAC) return mac;
	const mods = [...mac].filter((c) => c in PC_KEYS).map((c) => PC_KEYS[c]);
	const rest = [...mac].filter((c) => !(c in PC_KEYS)).join('');
	const order = ['Ctrl', 'Alt', 'Shift'];
	return [...order.filter((m) => mods.includes(m)), rest].join('+');
}
export const HELP_KEY = '/';
export const HELP_COMBO = `${MOD}${HELP_KEY}`;
export const MUTE_COMBO = IS_MAC ? '⇧⌘↓' : 'Ctrl+Shift+Down';

/** What a key event means, layout-independent for letters (⌥ turns letters into symbols on Mac). */
export const keyOf = (e: KeyboardEvent): string => {
	if (/^Key[A-Z]$/.test(e.code) && (e.altKey || !/^[a-z]$/i.test(e.key))) return e.code.slice(3).toLowerCase();
	if (/^Digit\d$/.test(e.code) && e.altKey) return e.code.slice(5);
	const k = e.key;
	if (k.length === 1 && /[a-z]/i.test(k)) return k.toLowerCase();
	if (e.code === 'Period') return '.';
	if (e.code === 'Comma') return ',';
	if (e.code === 'Slash') return '/';
	return k;
};

/** Somewhere a bare key is a character, not a command. */
export const typing = (t: EventTarget | null) =>
	t instanceof HTMLElement &&
	(t.isContentEditable ||
		['TEXTAREA', 'SELECT'].includes(t.tagName) ||
		(t instanceof HTMLInputElement && t.type !== 'range'));

const KEEPS_FOCUS =
	'[role=dialog],[role=alertdialog],[role=menu],[role=listbox],[role=grid],[role=slider],[data-selected],[data-track-selection]';

/** Handlers that live in the shell (injected so this module has no shell imports). */
export type ShellKeys = {
	focusSearch(): void;
	librarySearch(): void;
	libraryFilter(f: 'playlist' | 'artist' | 'album' | 'podcast' | null): void;
	toggleLeft(): void;
	nudgeLeft(d: number): void;
	nudgeRight(d: number): void;
	toggleRight(view: 'now-playing' | 'queue' | 'devices'): void;
	newPlaylist(): void;
	newFolder(): void;
	like(): void;
	goToContext(): void;
	settings(): void;
	/** ⌘F "Filter in current list" (native menu `find`); bound here only without the menu. */
	find?(): void;
};

// Up to the core's 3x (player::SPEED_RANGE), the podcast menu's top speed.
const SPEEDS = [0.5, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2, 2.5, 3];
function stepSpeed(dir: number) {
	const i = SPEEDS.findIndex((s) => s >= playback.speed - 1e-6);
	const next = SPEEDS[Math.min(SPEEDS.length - 1, Math.max(0, (i < 0 ? 4 : i) + dir))];
	setTempoPitch(next, playback.semitones);
}

export function initShortcuts(mini = false, shell?: ShellKeys) {
	const native = isTauri() && IS_MAC; // the native menu bar exists
	const onKey = (e: KeyboardEvent) => {
		if (e.defaultPrevented) return;
		const k = keyOf(e);
		const t = typing(e.target);
		const alt = e.altKey,
			sh = e.shiftKey,
			cmd = e.metaKey || e.ctrlKey;

		// ── bare keys ──
		if (!alt && !cmd) {
			if (t) return;
			if (k === ' ' && !sh) {
				// Native menu owns Space too, but the page sees it first: handle it here once.
				api.togglePause();
			} else if (sh && k === ',') stepSpeed(-1);
			else if (sh && k === '.') stepSpeed(1);
			// '?' is ⇧/ on most layouts and keyOf() maps the Slash key to '/', so match both.
			else if ((k === '?' || (sh && k === '/')) && !mini) ui.shortcutsOpen = !ui.shortcutsOpen;
			else return;
			e.preventDefault();
			return;
		}

		if (mini || !shell) {
			if (!native && alt && !sh && !cmd) {
				if (k === 's') api.toggleShuffle();
				else if (k === 'r') cycleRepeat();
				else return;
				e.preventDefault();
			}
			return;
		}

		// ── ⌥ (no ⇧, no ⌘): transport ──
		if (alt && !sh && !cmd) {
			if (k === 'ArrowLeft') api.prevTrack();
			else if (k === 'ArrowRight') api.nextTrack();
			else if (k === 'ArrowUp') nudgeVolume(10);
			else if (k === 'ArrowDown') nudgeVolume(-10);
			else if (!native && k === 's') api.toggleShuffle();
			else if (!native && k === 'r') cycleRepeat();
			else return;
			e.preventDefault();
			return;
		}

		// ── ⌥⇧ : navigation / layout ──
		if (alt && sh && !cmd) {
			switch (k) {
				case 'p': shell.newPlaylist(); break;
				case 'h': goto('/'); break;
				case 'm': goto('/made-for-you'); break; // stream: smart2
				case 'j': shell.goToContext(); break;
				case 's': goto(`/playlist/${api.LIKED_MUSIC_ID}`); break;
				case 'y': goto('/history'); break; // a11y: Spotify's "Recents" page
				case 'n': goto('/new-releases'); break; // a11y: Spotify ⌥⇧N
				case 'c': goto('/charts'); break; // a11y: Spotify ⌥⇧C
				case '0': shell.libraryFilter(null); break;
				case '1': shell.libraryFilter('playlist'); break;
				case '2': shell.libraryFilter('podcast'); break;
				case '3': shell.libraryFilter('artist'); break;
				case '4': shell.libraryFilter('album'); break;
				case 'd': shell.toggleRight('devices'); break;
				case 'ArrowLeft': shell.nudgeLeft(-20); break;
				case 'ArrowRight': shell.nudgeLeft(20); break;
				case 'ArrowDown': shell.nudgeRight(-20); break;
				case 'ArrowUp': shell.nudgeRight(20); break;
				// Menu-owned in the app; bound only without the native menu.
				case 'l': if (native) return; shell.toggleLeft(); break;
				case 'r': if (native) return; shell.toggleRight('now-playing'); break;
				case 'q': if (native) return; shell.toggleRight('queue'); break;
				case 'b': if (native) return; shell.like(); break;
				default: return;
			}
			e.preventDefault();
			return;
		}

		// ── ⌘ combos ──
		if (cmd) {
			if (sh && alt && k === 'f') shell.librarySearch();
			else if (sh && alt && k === 'p') shell.newFolder();
			else if (sh && !alt && k === 'l') shell.focusSearch();
			else if (native) return;
			// Browser fallback for menu-owned combos.
			else if (!sh && !alt && k === 'k') shell.focusSearch();
			else if (!sh && !alt && k === '/') ui.shortcutsOpen = !ui.shortcutsOpen;
			else if (!sh && !alt && k === ',') shell.settings();
			else if (!sh && !alt && k === 'f' && shell.find) shell.find();
			else if (!sh && !alt && k === 'n') shell.newPlaylist();
			else if (!sh && alt && k === 'n') shell.newFolder(); // a11y: native-menu ⌥⌘N, browser fallback
			else if (sh && !alt && k === 'ArrowDown' && !t) toggleMute(); // native-menu ⇧⌘↓, fallback
			else if (!sh && !alt && k === 'ArrowLeft' && !t) history.back();
			else if (!sh && !alt && k === 'ArrowRight' && !t) history.forward();
			else if (sh && !alt && k === 'ArrowLeft' && !t) api.seek(Math.max(0, playback.position - 10));
			else if (sh && !alt && k === 'ArrowRight' && !t) api.seek(playback.position + 10);
			else return;
			e.preventDefault();
		}
	};

	// Focus a click leaves behind is dropped (Limusic #551), except in fields/menus/lists/dialogs.
	let pointer = true;
	const onPointer = () => (pointer = true);
	const onTab = (e: KeyboardEvent) => {
		if (e.key === 'Tab') pointer = false;
	};
	const onFocus = (e: FocusEvent) => {
		const t = e.target;
		if (!pointer || !(t instanceof HTMLElement) || typing(t)) return;
		if (t.closest(KEEPS_FOCUS)) return;
		setTimeout(() => document.activeElement === t && t.blur());
	};
	window.addEventListener('keydown', onKey);
	window.addEventListener('pointerdown', onPointer, true);
	window.addEventListener('keydown', onTab, true);
	window.addEventListener('focusin', onFocus);
	return () => {
		window.removeEventListener('keydown', onKey);
		window.removeEventListener('pointerdown', onPointer, true);
		window.removeEventListener('keydown', onTab, true);
		window.removeEventListener('focusin', onFocus);
	};
}
