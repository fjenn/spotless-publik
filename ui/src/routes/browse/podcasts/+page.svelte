<script lang="ts">
	import { focusSearch } from '$lib/shell/search.svelte';
	/** Open search scoped to podcasts with the field focused: what you type next searches shows
	 *  (the top bar keeps `&type=` while you type), instead of landing on Browse all with no cursor. */
	async function searchPodcasts() {
		await goto('/search?type=podcasts');
		focusSearch();
	}

	/** Browse ▸ Podcasts: your shows, new episodes, continue listening, then the podcast chart. */
	import { onMount } from 'svelte';
	import type { ChartsPage } from '$lib/api';
	import { Card, Shelf, Skeleton, EmptyState } from '$lib/kit';
	import { showHref } from '$lib/kit/actions';
	import { podcasts } from '$lib/podcasts.svelte';
	import { goto } from '$app/navigation';
	import BrowseHeader from '$lib/discover/BrowseHeader.svelte';
	import EpisodeCard from '$lib/discover/EpisodeCard.svelte';
	import { TILE, chartsKey, loadCharts, peek } from '$lib/discover/data';
	import { exploreHref, isPlaying, playBrowse, toggle } from '$lib/discover/play.svelte';

	let charts = $state<ChartsPage | null>(peek<ChartsPage>(chartsKey()));
	let chartsFailed = $state(false);
	onMount(() => {
		loadCharts()
			.then((c) => (charts = c))
			.catch(() => (chartsFailed = true));
	});
	const top = $derived(charts?.sections.find((s) => s.kind === 'top_podcasts'));
	const showItem = (s: { id: string; title: string; author: string; thumbnail?: string | null }) => ({
		kind: 'podcast' as const,
		id: s.id,
		title: s.title,
		subtitle: s.author,
		thumbnail: s.thumbnail ?? undefined
	});
</script>

<div class="relative isolate min-h-full pb-10">
	<BrowseHeader title="Podcasts" color={TILE.podcasts} eyebrow="Browse" />
	<div class="flex flex-col gap-8 px-[var(--content-spacing)]">
		{#if podcasts.follows.length}
			<Shelf title="Your shows" items={podcasts.follows} href="/list?podcasts=shows&title=Your%20shows">
				{#snippet card(s)}
					{@const it = showItem(s)}
					<Card title={s.title} subtitle={s.author} image={s.thumbnail} kind="podcast" href={showHref(s.id)}
						playing={isPlaying(s.id)} onPlay={() => toggle(s.id, () => playBrowse(it))} />
				{/snippet}
			</Shelf>
		{/if}
		{#if podcasts.newEpisodes.length}
			<Shelf title="New episodes" items={podcasts.newEpisodes} href="/list?podcasts=new&title=New%20episodes">
				{#snippet card(e)}<EpisodeCard episode={e} />{/snippet}
			</Shelf>
		{/if}
		{#if podcasts.continueListening.length}
			<Shelf title="Continue listening" items={podcasts.continueListening} href="/list?podcasts=continue&title=Continue%20listening">
				{#snippet card(e)}<EpisodeCard episode={e} />{/snippet}
			</Shelf>
		{/if}
		{#if top}
			<Shelf title={top.title} items={top.items} rows={2}>
				{#snippet card(i)}
					<Card title={i.title} subtitle={i.subtitle} image={i.thumbnail} kind="podcast" href={exploreHref(i)}
						playing={isPlaying(i.id)} onPlay={() => toggle(i.id, () => playBrowse(i))} />
				{/snippet}
			</Shelf>
		{:else if !charts && !chartsFailed}
			<section aria-hidden="true" class="flex flex-col gap-3">
				<Skeleton width="220px" height={20} />
				<div class="-mx-3 grid grid-cols-5 overflow-hidden"><Skeleton variant="card" count={5} /></div>
			</section>
		{/if}
		{#if !podcasts.follows.length && !top && (charts || chartsFailed)}
			<EmptyState title="Let's find some podcasts to follow" body="Search for a show you like and follow it to see new episodes here."
				actionLabel="Search podcasts" onAction={searchPodcasts} />
		{/if}
	</div>
</div>
