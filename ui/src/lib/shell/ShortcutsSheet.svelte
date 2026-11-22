<script lang="ts">
	/** Keyboard shortcuts sheet (overlays.md, re-measured live in compare/PARITY-AUDIT.md P2-15):
	 *  ⌘/ or ?, 570px, #1f1f1f; a compact header (title 24/700 with the X on its line, the toggle
	 *  hint, a divider), sections 20/700, 14px rows with kbd chips; "Shift" is spelled out like
	 *  Spotify. Keys are Spotless's real bindings (shortcuts.ts + native menu). */
	import Modal from '$lib/kit/Modal.svelte';
	import Icon from '$lib/kit/Icon.svelte';
	import XIcon from 'phosphor-svelte/lib/XIcon';
	import { Dialog } from 'bits-ui';
	import { ui } from '$lib/player.svelte';
	import { keyCap } from '$lib/shortcuts';

	type Row = [string, string[]];
	const SECTIONS: [string, Row[]][] = [
		[
			'Basic',
			[
				['Create new playlist', ['⌥', '⇧', 'P']],
				['Create new folder', ['⌘', '⌥', '⇧', 'P']],
				['Open context menu', ['⌥', 'J']],
				['Open Quick Search', ['⌘', 'K']],
				['Search in Your Library', ['⇧', '⌘', '⌥', 'F']],
				['Filter in current list', ['⌘', 'F']],
				['Settings', ['⌘', ',']]
			]
		],
		[
			'Playback',
			[
				['Play / Pause', ['Space']],
				['Like', ['⌥', '⇧', 'B']],
				['Shuffle', ['⌥', 'S']],
				['Repeat', ['⌥', 'R']],
				['Skip to previous', ['⌥', '←']],
				['Skip to next', ['⌥', '→']],
				['Seek backward', ['⇧', '⌘', '←']],
				['Seek forward', ['⇧', '⌘', '→']],
				['Raise volume', ['⌥', '↑']],
				['Lower volume', ['⌥', '↓']],
				['Mute', ['⇧', '⌘', '↓']],
				['Decrease playback speed', ['⇧', ',']],
				['Increase playback speed', ['⇧', '.']],
				['Mini player', ['⌥', '⌘', 'M']]
			]
		],
		[
			'Navigation',
			[
				['Home', ['⌥', '⇧', 'H']],
				['Back in history', ['⌘', '←']],
				['Forward in history', ['⌘', '→']],
				['Currently playing', ['⌥', '⇧', 'J']],
				['Search', ['⌘', '⇧', 'L']],
				['Liked Songs', ['⌥', '⇧', 'S']],
				['Made for you', ['⌥', '⇧', 'M']],
				['New releases', ['⌥', '⇧', 'N']],
				['Charts', ['⌥', '⇧', 'C']],
				['Queue', ['⌥', '⇧', 'Q']],
				['Your Library', ['⌥', '⇧', '0']],
				['Your playlists', ['⌥', '⇧', '1']],
				['Your podcasts', ['⌥', '⇧', '2']],
				['Your artists', ['⌥', '⇧', '3']],
				['Your albums', ['⌥', '⇧', '4']],
				['Connect to a device', ['⌥', '⇧', 'D']],
				['Recents', ['⌥', '⇧', 'Y']]
			]
		],
		[
			'Layout',
			[
				['Toggle left sidebar', ['⌥', '⇧', 'L']],
				['Decrease left sidebar width', ['⌥', '⇧', '←']],
				['Increase left sidebar width', ['⌥', '⇧', '→']],
				['Toggle right sidebar', ['⌥', '⇧', 'R']],
				['Decrease right sidebar width', ['⌥', '⇧', '↓']],
				['Increase right sidebar width', ['⌥', '⇧', '↑']],
				['Zoom in / out / reset', ['⌘', '=', '-', '0']]
			]
		]
	];
</script>

<Modal
	bind:open={ui.shortcutsOpen}
	width={570}
	closeButton={false}
	panelClass="!bg-bg-elevated !p-0 max-h-[calc(100vh-96px)]"
>
	<div class="sticky top-0 z-[1] border-b border-white/10 bg-bg-elevated px-6 pt-6 pb-4">
		<div class="flex items-center justify-between gap-4">
			<Dialog.Title class="font-title text-xl font-bold text-white">Keyboard shortcuts</Dialog.Title>
			<Dialog.Close
				class="-mr-2 grid size-8 shrink-0 place-items-center rounded-full text-text-subdued transition-[color,transform] duration-100 hover:scale-[1.04] hover:text-white"
				aria-label="Close"
			>
				<Icon icon={XIcon} size={18} />
			</Dialog.Close>
		</div>
		<p class="mt-2 flex flex-wrap items-center gap-1 text-sm text-white">
			Press <kbd class="key">{keyCap('⌘')}</kbd><kbd class="key">/</kbd> or <kbd class="key">?</kbd> to toggle this modal.
		</p>
	</div>
	<div class="px-6 pb-6">
		{#each SECTIONS as [name, rows] (name)}
			<h3 class="mt-6 mb-2 font-title text-lg font-bold text-white">{name}</h3>
			{#each rows as [label, keys] (label)}
				<div class="flex min-h-10 items-center justify-between gap-4">
					<span class="text-sm text-white">{label}</span>
					<span class="flex gap-1">{#each keys as k, i (i)}<kbd class="key">{k === '⇧' ? 'Shift' : keyCap(k)}</kbd>{/each}</span>
				</div>
			{/each}
		{/each}
	</div>
</Modal>

<style>
	/* Spotify's sheet chips: the global .kbd outline at 14px. */
	.key {
		display: inline-grid;
		place-items: center;
		min-width: 24px;
		height: 24px;
		padding: 0 5px;
		border-radius: 4px;
		font: inherit;
		font-size: 14px;
		line-height: 20px;
		color: #fff;
		box-shadow:
			0 1px 0 #121212,
			inset 0 0 0 1px #7c7c7c;
	}
</style>
