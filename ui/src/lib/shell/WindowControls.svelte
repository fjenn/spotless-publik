<script lang="ts">
	/**
	 * Minimize / maximize / close for Windows and Linux, where the window is undecorated
	 * (`decorations: false`) and the top bar is the title bar. Spotify-desktop placement: flush in
	 * the top-right corner, 46×32 caption buttons, thin glyphs, red hover on close. Hidden when the
	 * compositor draws the frame (`win.chrome !== 'off'`: macOS traffic lights or the "system title
	 * bar" setting). Minimize and maximize follow the desktop's GTK button layout on Linux (#481).
	 */
	import { getCurrentWindow } from '@tauri-apps/api/window';
	import { win } from '$lib/win.svelte';

	const w = (() => {
		try {
			return getCurrentWindow();
		} catch {
			return null;
		}
	})();
	const btn =
		'grid h-8 w-[46px] place-items-center text-text-subdued transition-colors hover:bg-white/10 hover:text-white';
</script>

{#if win.chrome === 'off'}
	<div class="-mr-4 ml-2 flex items-start self-stretch" data-window-control>
		{#if win.minimize}
			<button type="button" class={btn} aria-label="Minimize" onclick={() => w?.minimize()}>
				<svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M0 5.5h10" stroke="currentColor" /></svg>
			</button>
		{/if}
		{#if win.maximize}
			<button
				type="button"
				class={btn}
				aria-label={win.maximized ? 'Restore' : 'Maximize'}
				onclick={() => w?.toggleMaximize()}
			>
				{#if win.maximized}
					<svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" fill="none" stroke="currentColor">
						<path d="M2.5 2.5V.5h7v7h-2" /><rect x=".5" y="2.5" width="7" height="7" />
					</svg>
				{:else}
					<svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" fill="none" stroke="currentColor">
						<rect x=".5" y=".5" width="9" height="9" />
					</svg>
				{/if}
			</button>
		{/if}
		<button
			type="button"
			class="grid h-8 w-[46px] place-items-center text-text-subdued transition-colors hover:bg-[#e81123] hover:text-white"
			aria-label="Close"
			onclick={() => w?.close()}
		>
			<svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" stroke="currentColor"><path d="M.5.5l9 9M9.5.5l-9 9" /></svg>
		</button>
	</div>
{/if}
