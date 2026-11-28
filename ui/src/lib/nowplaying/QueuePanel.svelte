<script lang="ts">
	/**
	 * Queue view of the right panel (queue-panel.md): "Now playing" (one row), "Next in queue" (the
	 * hand-queued rows) with "Clear queue" (instant, Undo toast), "Next from: <context>" (rest of the album /
	 * playlist), then autoplay's "Next up" with its on/off hint. The rows before the playing one stay
 * folded behind "Show N earlier songs" above "Now playing" (issue #3): a click plays them, they
 * can't be dragged or removed.
	 * Drag to reorder upcoming rows (Limusic's `moveTarget` + `move_in_queue`), click selects,
	 * Delete/Backspace removes the selection, double-click / cover play plays, right-click opens
	 * the queue item menu. Rows playback skips (`skipReasons`, `unavailable`) are dimmed and inert.
	 * Long contexts render in pages of 60 as you scroll.
	 *
	 * Lane magic-d2: songs, selections and album/playlist cards dragged in from anywhere drop into
	 * "Next in queue" at the accent line (`add_to_queue_at`); a "Recently played" tab sits beside
	 * the queue; Enhance's suggestions show with their sparkle when that playlist is playing.
	 */
	import { goto } from '$app/navigation';
	import * as api from '$lib/api';
	import type { SongItem } from '$lib/api';
	import { playback, prefs, setAutoplay } from '$lib/player.svelte';
	import { moveTarget, queueView, type QueueRow as Row } from '$lib/queue';
	import {
		dragScroll,
		isTracksDrag,
		queueDropAt,
		queueDropRange,
		QUEUE_ROW_MIME,
		readTrackDrop,
		SONGS_MIME,
		TRACKS_MIME
	} from '$lib/dnd';
	import { addToQueueAt, resolveTrackDrop, restoreQueued } from '$lib/dndTracks';
	import { toast, toastUndo } from '$lib/kit/toast';
	import { t } from '$lib/i18n.svelte';
	import { lib } from '$lib/library.svelte';
	import type { RecentTrack } from '$lib/api';
	// Lane magic-d2: Enhance suggestions in the queue keep their sparkle (magic-d3's store).
	import { addEnhanced, enhance, enhanceEligible, enhancedRecs, loadEnhance, removeEnhanced } from '$lib/smart/enhance.svelte';
	import { openQueueItemMenu, openTrackMenu } from '$lib/kit/menus';
	import { removeFromQueue } from '$lib/kit/actions';
	import QueueRow from './QueueRow.svelte';
	import { roving } from '$lib/kit/roving';
	// ── stream: smart2 ── Smart Shuffle suggestions: sparkle + add/remove instead of a plain row.
	import SuggestionRow from '$lib/smart/SuggestionRow.svelte';
	import { addSuggestion, removeSuggestion, smartShuffle } from '$lib/smart/smartshuffle.svelte';


	const view = $derived(queueView(playback.queue));
	const cur = $derived(playback.queue.currentIndex);
	const nowRow = $derived(view.rows.find((r) => r.i === cur) ?? null);
	const upcoming = $derived(view.rows.filter((r) => r.i > cur));
	const queued = $derived(upcoming.filter((r) => r.item.queued || r.item.queued_end));
	const context = $derived(upcoming.filter((r) => !r.item.queued && !r.item.queued_end));
	const autoplayOn = $derived(playback.queue.autoplay ?? prefs.autoplay);
	const skips = $derived(playback.queue.skipReasons ?? {});
	const contextName = $derived(playback.queue.sourceName ?? null);

	// Paging: the queue of a big playlist can hold thousands of rows.
	const PAGE = 60;
	let limit = $state(PAGE);
	$effect(() => {
		playback.queue.sourceName;
		limit = PAGE;
	});
	// The earlier rows fold again only when something else starts playing, not on every track.
	let earlierFor: string | null = null;
	$effect(() => {
		const key = playback.queue.sourceId ?? playback.queue.sourceName ?? null;
		if (key === earlierFor) return;
		earlierFor = key;
		showEarlier = false;
		earlierLimit = PAGE;
	});
	const contextShown = $derived(context.slice(0, limit));

	// Issue #3: the songs before the playing one are still in the queue (Previous walks them), so
	// they can be shown and played from here. Folded by default; the nearest ones first, in pages.
	const earlier = $derived(view.rows.filter((r) => r.i < cur));
	let showEarlier = $state(false);
	let earlierLimit = $state(PAGE);
	const earlierShown = $derived(showEarlier ? earlier.slice(-earlierLimit) : []);
	const autoplayShown = $derived(view.autoplay.slice(0, Math.max(0, limit - context.length)));
	const more = $derived(context.length + view.autoplay.length > limit);
	function sentinel(el: HTMLElement) {
		const root = el.closest('[data-right-scroll]');
		const io = new IntersectionObserver(
			(es) => {
				if (es.some((e) => e.isIntersecting)) limit += PAGE;
			},
			{ root, rootMargin: '400px' }
		);
		io.observe(el);
		return () => io.disconnect();
	}

	// ── selection & keys ──
	let selected = $state<number | null>(null);
	function onKey(e: KeyboardEvent) {
		if (selected === null) return;
		if ((e.key === 'Delete' || e.key === 'Backspace') && selected > cur) {
			e.preventDefault();
			const i = selected;
			selected = null;
			removeFromQueue(i);
		} else if (e.key === 'Escape') selected = null;
		else if (e.key === 'Enter') play(selected);
	}

	function play(i: number) {
		if (i === cur) api.togglePause();
		else api.playIndex(i);
	}

	function menu(e: MouseEvent | HTMLElement, r: Row) {
		selected = r.i;
		if (r.i > cur) openQueueItemMenu(e, r.item, r.i);
		else openTrackMenu(e, [r.item], { playlistId: playback.queue.sourceId ?? undefined });
	}

	// Spotify clears at once, no confirmation. The rows are snapshotted first, so Undo can put them
	// back (`restore_queued`).
	async function clear() {
		const items = playback.queue.items;
		const snap = $state.snapshot(
			items.filter((s, i) => i > cur && (s.queued || s.queued_end))
		) as SongItem[];
		if (!snap.length) return;
		try {
			await api.clearQueued();
		} catch (e) {
			toast.error(String(e));
			return;
		}
		toastUndo('Queue cleared', () => restoreQueued(snap));
	}

	// ── Enhance suggestions (magic-d3): the playing playlist's suggestions show their sparkle ──
	const sourceId = $derived(playback.queue.sourceId ?? null);
	$effect(() => {
		const id = sourceId;
		if (id && enhanceEligible(id) && !enhance.byId[id]) void loadEnhance(id);
	});
	const enhanceIds = $derived(
		new Set(sourceId && enhanceEligible(sourceId) ? enhancedRecs(sourceId).map((s) => s.video_id) : [])
	);

	// ── drag to reorder (upcoming rows only) ──
	let dragFrom = $state<number | null>(null);
	let dropAt = $state<number | null>(null);
	function onDragStart(e: DragEvent, r: Row) {
		if (!e.dataTransfer || r.i <= cur) return;
		e.dataTransfer.setData(QUEUE_ROW_MIME, String(r.i));
		e.dataTransfer.effectAllowed = 'move';
		dragFrom = r.i;
	}
	function onDragOver(e: DragEvent, r: Row) {
		if (dragFrom === null && isTracksDrag(e)) return overRowFromOutside(e, r);
		if (dragFrom === null || !e.dataTransfer?.types.includes(QUEUE_ROW_MIME)) return;
		e.preventDefault();
		e.dataTransfer.dropEffect = 'move';
		const box = (e.currentTarget as HTMLElement).getBoundingClientRect();
		dropAt = e.clientY < box.top + box.height / 2 ? r.i : r.i + 1;
		if (dropAt <= cur) dropAt = cur + 1;
	}
	function onDrop(e: DragEvent) {
		e.preventDefault();
		e.stopPropagation(); // the panel's own drop handler would add an outside drop twice
		if (extAt !== null || (dragFrom === null && isTracksDrag(e))) return dropFromOutside(e);
		const to = dragFrom !== null && dropAt !== null ? moveTarget(dragFrom, dropAt) : null;
		if (to !== null && dragFrom !== null) api.moveInQueue(dragFrom, to).catch(() => {});
		dragFrom = null;
		dropAt = null;
	}
	const lastUpcoming = $derived((view.autoplay.at(-1) ?? upcoming.at(-1))?.i ?? -1);

	// ── drops from outside (lane magic-d2) ──
	// Songs, a multi-selection or an album/playlist card dragged in from a page, search or the
	// library. They land in "Next in queue" (queue-panel.md: you drag *into* it); over the context
	// or autoplay rows the line parks at the end of that block, over the playing row right below it.
	// `extAt` is the queue index the dropped songs go in front of.
	let extAt = $state<number | null>(null);
	const dropRange = $derived(queueDropRange(playback.queue.items, cur));
	function overRowFromOutside(e: DragEvent, r: Row) {
		e.preventDefault();
		e.stopPropagation(); // the panel's own handler would park the line at the block's end
		if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
		const box = (e.currentTarget as HTMLElement).getBoundingClientRect();
		extAt = queueDropAt(dropRange, r.i, e.clientY < box.top + box.height / 2);
	}
	function overPanel(e: DragEvent) {
		if (dragFrom !== null || !isTracksDrag(e)) return;
		e.preventDefault();
		if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
		extAt = dropRange[1];
	}
	function leavePanel(e: DragEvent) {
		if (!(e.currentTarget as HTMLElement).contains(e.relatedTarget as Node)) extAt = null;
	}
	async function dropFromOutside(e: DragEvent) {
		e.preventDefault();
		const at = extAt ?? dropRange[1];
		extAt = null;
		const d = readTrackDrop(e); // now: the data is cleared once this event returns
		if (!d) return;
		try {
			const { songs, from } = await resolveTrackDrop(d);
			if (!songs.length) return;
			await addToQueueAt(songs, at, from);
			const n = songs.length;
			toast.success(n === 1 ? t('toasts.added_to_queue_one') : t('toasts.added_to_queue', { count: n }));
		} catch (err) {
			toast.error(String(err));
		}
	}
	/** Which edge of row `r` draws the outside-drop line: the top of a "Next in queue" row the
	 *  songs go in front of, or the bottom of the row the block ends with (or the playing row). */
	function extLine(r: Row): 'top' | 'bottom' | null {
		if (extAt === null) return null;
		const inBlock = r.i >= dropRange[0] && r.i < dropRange[1];
		if (inBlock && extAt === r.i) return 'top';
		if (extAt === r.i + 1 && (r.i === dropRange[1] - 1 || (r.i === cur && dropRange[0] === dropRange[1])))
			return 'bottom';
		return null;
	}

	// ── Recently played tab (gap 19): the last 50 plays, the /history page's query ──
	let tab = $state<'queue' | 'recent'>('queue');
	let recent = $state<RecentTrack[] | null>(null);
	let recentError = $state(false);
	async function loadRecent() {
		recentError = false;
		try {
			recent = await lib.recentTracks(50);
		} catch {
			recentError = true;
		}
	}
	$effect(() => {
		if (tab !== 'recent') return;
		void playback.now?.videoId; // a new song started: it is history now
		void loadRecent();
	});
	function playRecent(i: number) {
		const songs = (recent ?? []).map((r) => r.song);
		if (songs.length) api.playPlaylist(songs, i, undefined, 'Recently played');
	}
</script>

<svelte:window
	ondragend={() => {
		dragFrom = null;
		dropAt = null;
		extAt = null;
	}}
	ondrop={() => (extAt = null)}
/>

{#snippet row(r: Row)}
	<div
		class="relative {dragFrom === r.i ? 'opacity-40' : ''}"
		draggable={r.i > cur}
		ondragstart={(e) => onDragStart(e, r)}
		ondragover={(e) => onDragOver(e, r)}
		ondrop={onDrop}
		role="presentation"
	>
		{#if dropAt === r.i || extLine(r) === 'top'}
			<div class="pointer-events-none absolute inset-x-2 -top-px z-10 h-0.5 rounded-full bg-accent" data-drop-line></div>
		{:else if (dropAt === r.i + 1 && r.i === lastUpcoming) || extLine(r) === 'bottom'}
			<div class="pointer-events-none absolute inset-x-2 -bottom-px z-10 h-0.5 rounded-full bg-accent" data-drop-line></div>
		{/if}
		{#if r.i !== cur && smartShuffle.recIds.includes(r.item.video_id)}
			<SuggestionRow song={r.item} compact onAdd={addSuggestion} onRemove={removeSuggestion} />
		{:else if r.i !== cur && sourceId && !r.item.queued && !r.item.queued_end && enhanceIds.has(r.item.video_id)}
			<SuggestionRow
				song={r.item}
				compact
				onAdd={(s) => addEnhanced(sourceId, contextName ?? '', s)}
				onRemove={(s) => removeEnhanced(sourceId, s)}
			/>
		{:else}
		<QueueRow
			song={r.item}
			current={r.i === cur}
			paused={playback.paused}
			selected={selected === r.i}
			index={r.i}
			reason={r.item.reason}
			skip={skips[r.item.video_id]}
			onplay={() => play(r.i)}
			onmenu={(e) => menu(e, r)}
			onselect={() => (selected = r.i)}
		/>
		{/if}
	</div>
{/snippet}

{#snippet heading(text: string, action?: { label: string; run: () => void }, first = false)}
	<!-- parity3 P3-9: "Now playing" right under the panel header (no empty band) -->
	<div role="row" class="flex items-start justify-between gap-2 px-2 {first ? 'pt-2' : 'pt-[29px]'} pb-[7px]">
		<div role="rowheader" class="contents">
		<h3 class="min-w-0 text-base leading-[22px] font-bold text-white">{text}</h3>
		{#if action}
			<button
				type="button"
				class="shrink-0 text-sm font-bold text-text-subdued hover:text-white hover:underline"
				onclick={action.run}>{action.label}</button
			>
		{/if}
		</div>
	</div>
{/snippet}

<!-- Spotify's "Queue | Recently played" pills (queue-panel.md: 32px pills under the header). -->
<div class="flex gap-2 px-4 pt-2" role="tablist" aria-label="Queue view">
	{#each [['queue', 'Queue'], ['recent', 'Recently played']] as [id, label] (id)}
		<button
			type="button"
			role="tab"
			aria-selected={tab === id}
			class="h-8 rounded-full px-3 text-sm transition-colors {tab === id
				? 'bg-white text-black'
				: 'bg-white/[0.07] text-white hover:bg-white/[0.1]'}"
			onclick={() => (tab = id as 'queue' | 'recent')}>{label}</button
		>
	{/each}
</div>

{#if tab === 'recent'}
	<div class="px-2 pb-4 pt-2" role="list" aria-label="Recently played">
		{#if recentError}
			<p class="px-2 pt-8 text-center text-sm text-text-subdued">
				Couldn't load your recently played songs.
				<button type="button" class="font-bold text-white hover:underline" onclick={loadRecent}>Try again</button>
			</p>
		{:else if recent && !recent.length}
			<div class="flex flex-col items-center gap-2 px-4 pt-16 text-center">
				<h3 class="text-xl font-bold text-white">Nothing played yet</h3>
				<p class="text-base text-text-subdued">Songs you play will show up here.</p>
			</div>
		{:else if recent}
			{#each recent as r, i (r.song.video_id + ':' + r.playedAt)}
				<div role="listitem">
					<QueueRow
						song={r.song}
						current={playback.now?.videoId === r.song.video_id && i === 0}
						paused={playback.paused}
						onplay={() => playRecent(i)}
						onmenu={(e) => openTrackMenu(e, [r.song])}
					/>
				</div>
			{/each}
		{/if}
	</div>
{:else}
<!-- a11y: one Tab stop — the rows carry a roving tabindex (kit/roving). -->
<!-- svelte-ignore a11y_interactive_supports_focus -->
<div
	class="min-h-full px-2 pb-4 outline-none"
	role="grid"
	aria-label="Queue"
	ondragover={overPanel}
	ondragleave={leavePanel}
	ondrop={(e) => {
		if (extAt !== null || (dragFrom === null && isTracksDrag(e))) void dropFromOutside(e);
	}}
	onkeydown={onKey}
	onfocusin={(e) => {
		// a11y: the focused row (roving focus) is the keyboard selection.
		const el = e.target as HTMLElement;
		if (el.dataset.queueIndex !== undefined) selected = Number(el.dataset.queueIndex);
	}}
	{@attach roving({ items: '[data-queue-row]', orientation: 'vertical' })}
	{@attach (n) => dragScroll(n.closest('[data-right-scroll]') as HTMLElement ?? n, QUEUE_ROW_MIME)}
	{@attach (n) => dragScroll(n.closest('[data-right-scroll]') as HTMLElement ?? n, SONGS_MIME)}
	{@attach (n) => dragScroll(n.closest('[data-right-scroll]') as HTMLElement ?? n, TRACKS_MIME)}
>
	{#if earlier.length && nowRow}
		{#if showEarlier}
			{@render heading(
				contextName ? `Earlier from: ${contextName}` : 'Earlier',
				{ label: 'Hide', run: () => (showEarlier = false) },
				true
			)}
			{#if earlier.length > earlierLimit}
				<div role="row"><div role="gridcell" class="px-2 pb-2">
					<button
						type="button"
						class="text-sm font-bold text-text-subdued hover:text-white hover:underline"
						onclick={() => (earlierLimit += PAGE)}>Show more</button
					>
				</div></div>
			{/if}
			{#each earlierShown as r (r.key)}{@render row(r)}{/each}
		{:else}
			<div role="row"><div role="gridcell" class="px-2 pt-2">
				<button
					type="button"
					class="text-sm font-bold text-text-subdued hover:text-white hover:underline"
					data-queue-earlier
					onclick={() => (showEarlier = true)}
					>{earlier.length === 1 ? 'Show 1 earlier song' : `Show ${earlier.length} earlier songs`}</button
				>
			</div></div>
		{/if}
	{/if}

	{#if nowRow}
		{@render heading('Now playing', undefined, !earlier.length)}
		{@render row(nowRow)}
	{/if}

	{#if !upcoming.length && !view.autoplay.length}
		<div role="row" class="flex flex-col items-center gap-4 px-4 pt-16 text-center"><div role="gridcell" class="contents">
			<h3 class="text-xl font-bold text-white">Add to your queue</h3>
			<p class="text-base text-text-subdued">
				Tap ‘Add to queue’ from a track's menu to see it here
			</p>
			<button
				type="button"
				class="h-8 rounded-full bg-white px-4 text-sm font-bold text-black transition-transform hover:scale-[1.04] active:scale-100"
				onclick={() => goto('/search')}>Find something to play</button
			>
			{#if nowRow && !autoplayOn}
				<p class="mt-4 text-sm text-text-subdued">
					Autoplay is off.
					<button type="button" class="font-bold text-white hover:underline" onclick={() => setAutoplay(true)}
						>Turn on</button
					>
					to keep listening when your queue ends.
				</p>
			{/if}
		</div></div>
	{:else}
		{#if queued.length}
			{@render heading('Next in queue', { label: 'Clear queue', run: clear })}
			{#each queued as r (r.key)}{@render row(r)}{/each}
		{/if}

		{#if contextShown.length}
			{@render heading(contextName ? `Next from: ${contextName}` : 'Next up')}
			{#each contextShown as r (r.key)}{@render row(r)}{/each}
		{/if}

		{#if autoplayShown.length}
			{#if view.upNext}
				<!-- Podcast up next: the core's predicted episodes, each row showing its reason. -->
				{@render heading('Next up')}
			{:else}
				{@render heading('Next: Autoplay')}
				<div role="row"><div role="gridcell" class="-mt-1 px-2 pb-2 text-sm text-text-subdued">
					Autoplay keeps similar music going.
					<button type="button" class="font-bold text-white hover:underline" onclick={() => setAutoplay(false)}
						>Turn off</button
					>
				</div></div>
			{/if}
			{#each autoplayShown as r (r.key)}{@render row(r)}{/each}
		{:else if !autoplayOn && !more}
			<div role="row"><div role="gridcell" class="px-2 pt-6 text-sm text-text-subdued">
				Autoplay is off.
				<button type="button" class="font-bold text-white hover:underline" onclick={() => setAutoplay(true)}
					>Turn on</button
				>
				to keep listening when your queue ends.
			</div></div>
		{/if}

		{#if more}
			<div class="h-px" aria-hidden="true" {@attach sentinel}></div>
		{/if}
	{/if}
</div>
{/if}
