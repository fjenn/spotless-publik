<script lang="ts">
	import { offersLyrics } from '$lib/nowlyrics.svelte';
	import { displayRuns } from '$lib/artistruns';
	/**
	 * Now Playing view of the right panel (now-playing-panel.md). Padding 0 16 16, sections 16 apart:
	 *  1. artwork 1:1 radius 8 — or the music video (VideoSurface's element) when video mode is on,
	 *     with Spotify's "Switch to video" / "Switch to audio" pill under it;
	 *  2. title 24/700 (album link, marquee on overflow) + artists 14px links; like / add button;
	 *  3. lyrics preview card;
	 *  4. About the artist — or, for a podcast episode, the episode description + playback speed;
	 *  5. Next in queue + "Open queue".
	 *  4b. Credits (lane magic-d6): the people behind a song, Follow, "Show all" → the Credits dialog;
 * Content cross-fades (200ms) on track change. On tour / Videos are LATER.
	 */
	import { fade } from 'svelte/transition';
	import * as api from '$lib/api';
	import { playback, ui } from '$lib/player.svelte';
	import { thumb } from '$lib/thumb';
	import { nowPlayingSong } from '$lib/shell/nowplaying';
	import { openRight } from '$lib/shell/layout.svelte';
	import Icon from '$lib/kit/Icon.svelte';
	import IconButton from '$lib/kit/IconButton.svelte';
	import EmptyState from '$lib/kit/EmptyState.svelte';
	import { albumHref, artistHref, isLiked, showHref, toggleLike } from '$lib/kit/actions';
	import { openTrackMenu } from '$lib/kit/menus';
	import Marquee from '$lib/components/Marquee.svelte';
	import { canSwitch, pickVersion, showVideo, videoSide, video } from '$lib/video.svelte';
	import LyricsPreviewCard from './LyricsPreviewCard.svelte';
	import ArtistCard from './ArtistCard.svelte';
	import CreditsCard from './CreditsCard.svelte';
	import EpisodeCard from './EpisodeCard.svelte';
	import ChaptersCard from './ChaptersCard.svelte';
	import BookChaptersCard from '$lib/audiobooks/BookChaptersCard.svelte'; // stream: audiobooks
	import { nowChapterIndex, nowChapters } from '$lib/chapters.svelte';
	import QueueRow from './QueueRow.svelte';
	import VideoBox from './VideoBox.svelte';
	import { episodeInfo } from './info';
	import PlusCircleIcon from 'phosphor-svelte/lib/PlusCircleIcon';
	import CheckCircleIcon from 'phosphor-svelte/lib/CheckCircleIcon';
	import MonitorPlayIcon from 'phosphor-svelte/lib/MonitorPlayIcon';
	import MusicNotesIcon from 'phosphor-svelte/lib/MusicNotesIcon';

	const now = $derived(playback.now);
	const song = $derived(now ? nowPlayingSong() : null);
	const local = $derived(!!now && api.isLocalId(now.videoId));
	const liked = $derived(song ? isLiked(song) : false);
	const episode = $derived(!!now?.isEpisode);
	const albumId = $derived(song && !local ? song.album_id : undefined);
	const runs = $derived(now ? displayRuns(now.artistRuns, now.artists, now.artistId) : []);
	const artistId = $derived(now?.artistId ?? now?.artistRuns?.find((r) => r.id)?.id);
	// A podcast episode: its show (link + description card) from the podcasts backend.
	let ep = $state<api.PodcastEpisode | null>(null);
	$effect(() => {
		const id = episode ? now?.videoId : undefined;
		ep = null;
		if (!id) return;
		let stale = false;
		episodeInfo(id)
			.then((e) => !stale && (ep = e))
			.catch(() => {});
		return () => (stale = true);
	});
	const showId = $derived(ep?.showId ?? null);
	const chapter = $derived(episode ? nowChapters.list[nowChapterIndex()] : undefined);

	const nextIndex = $derived(playback.queue.currentIndex + 1);
	const next = $derived(playback.queue.items[nextIndex] ?? null);
	const videoOn = $derived(showVideo());
</script>

{#if now}
	{#key now.videoId}
		<div class="flex flex-col gap-4 px-4 pb-4" in:fade={{ duration: 200 }}>
			<div class="flex flex-col gap-3">
				{#if videoOn}
					<!-- Click the picture for Full screen (the player bar's corners button does the same). -->
					<button
						type="button"
						class="block w-full cursor-pointer"
						aria-label="Enter Full screen"
						title="Enter Full screen"
						onclick={() => (ui.theaterOpen = true)}
					>
						<VideoBox fit="contain" class="pointer-events-none aspect-video w-full rounded-lg" />
					</button>
				{:else}
					<div class="aspect-square w-full overflow-hidden rounded-lg bg-bg-menu shadow-card-image">
						{#if now.thumbnail}
							<img src={thumb(now.thumbnail, 600)} alt="" class="size-full object-cover" />
						{:else}
							<div class="grid size-full place-items-center text-text-subdued">
								<Icon icon={MusicNotesIcon} size={64} />
							</div>
						{/if}
					</div>
				{/if}
				{#if canSwitch()}
					<button
						type="button"
						class="flex h-8 items-center gap-2 self-start rounded-full bg-bg-tinted px-3 text-sm font-bold text-white transition-transform hover:scale-[1.04] hover:bg-bg-tinted-highlight active:scale-100 disabled:opacity-50"
						disabled={video.switching !== null}
						onclick={() => pickVersion(!videoSide())}
					>
						<Icon icon={MonitorPlayIcon} size={16} />
						{videoSide() ? 'Switch to audio' : 'Switch to video'}
					</button>
				{/if}
			</div>

			<!-- pb-3: the first card sits 29px under the artist line (Spotify, measured). -->
			<div class="flex items-center gap-2 pb-3">
				<div class="min-w-0 flex-1" role="presentation" oncontextmenu={(e) => song && openTrackMenu(e, [song])}>
					<Marquee text={now.title} class="text-xl leading-[29px] font-bold text-white">
						{#if albumId}
							<a href={albumHref(albumId)} class="hover:underline">{now.title}</a>
						{:else}
							{now.title}
						{/if}
					</Marquee>
					<div class="truncate text-sm text-text-subdued">
						{#if episode && showId}
							<a href={showHref(showId)} class="hover:text-white hover:underline">{now.artists}</a>
						{:else}
							{#each runs as r, i (i)}
								{#if i > 0}<span>, </span>{/if}
								{#if r.id}
									<a href={artistHref(r.id)} class="hover:text-white hover:underline">{r.text}</a>
								{:else}<span>{r.text}</span>{/if}
							{/each}
						{/if}
					</div>
					{#if chapter}
						<div class="truncate text-sm text-white" data-testid="np-chapter">{chapter.title}</div>
					{/if}
				</div>
				{#if song}
					<IconButton
						icon={liked ? CheckCircleIcon : PlusCircleIcon}
						fill={liked}
						active={liked}
						label={liked ? 'Remove from Liked Songs' : 'Add to Liked Songs'}
						onclick={() => song && toggleLike(song)}
					/>
				{/if}
			</div>

			{#if offersLyrics(now)}
				<LyricsPreviewCard />
			{/if}

			{#if episode}
				<BookChaptersCard id={now.videoId} /><!-- stream: audiobooks -->
				<ChaptersCard />
				<EpisodeCard {ep} />
			{:else if artistId && !local}
				<ArtistCard {artistId} />
			{/if}

			{#if song && !episode && !local}
				<CreditsCard {song} />
			{/if}

			{#if next}
				<div class="rounded-lg bg-bg-highlight p-4">
					<div class="flex items-center justify-between gap-2 pb-2">
						<h3 class="text-base font-bold text-white">{next.reason ? 'Next up' : 'Next in queue'}</h3>
						<button
							type="button"
							class="text-sm font-bold text-text-subdued hover:text-white hover:underline"
							onclick={() => openRight('queue')}>Open queue</button
						>
					</div>
					<div class="-mx-2" role="grid" aria-label="Next in queue">
						<QueueRow
							song={next}
							reason={next.reason}
							skip={playback.queue.skipReasons?.[next.video_id]}
							onplay={() => api.playIndex(nextIndex)}
							onmenu={(e) => openTrackMenu(e, [next], { queueIndex: nextIndex })}
						/>
					</div>
				</div>
			{/if}
		</div>
	{/key}
{:else}
	<div class="px-4 pt-16">
		<EmptyState size="sm" title="Nothing playing yet" body="Pick a song, album or podcast and it shows up here." />
	</div>
{/if}
