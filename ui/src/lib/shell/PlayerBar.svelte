<script lang="ts">
	import { offersLyrics } from '$lib/nowlyrics.svelte';
	import { displayRuns } from '$lib/artistruns';
	/**
	 * Player bar (docs/spotify-spec/screens/player-bar.md): 72px on the app black, three zones
	 * 30% / 40% / 30%. Left: 56px cover (toggles the Now Playing view), title/artists, like.
	 * Centre: shuffle · previous · play · next · repeat, then time · progress · duration.
	 * Right: Now Playing view · lyrics · queue · devices · mute + volume · mini player · fullscreen.
	 */
	import * as api from '$lib/api';
	import type { SongItem } from '$lib/api';
	import {
		commitVolume,
		cycleRepeat,
		dragVolume,
		openMiniPlayer,
		playback,
		SKIP_SECS,
		skipBy,
		toggleMute,
		ui,
		wheelVolume
	} from '$lib/player.svelte';
	import { thumb } from '$lib/thumb';
	import Icon from '$lib/kit/Icon.svelte';
	import Marquee from '$lib/components/Marquee.svelte';
	import IconButton from '$lib/kit/IconButton.svelte';
	import Slider from '$lib/kit/Slider.svelte';
	import Tooltip from '$lib/kit/Tooltip.svelte';
	import { albumHref, artistHref, isLiked } from '$lib/kit/actions';
	import { openSavedIn } from '$lib/kit/menus';
	import { openTrackMenu } from '$lib/kit/menus';
	import { shell, toggleLyrics, toggleRight } from './layout.svelte';
	import ShuffleIcon from 'phosphor-svelte/lib/ShuffleIcon';
	import SkipBackIcon from 'phosphor-svelte/lib/SkipBackIcon';
	import SkipForwardIcon from 'phosphor-svelte/lib/SkipForwardIcon';
	import PlayIcon from 'phosphor-svelte/lib/PlayIcon';
	import PauseIcon from 'phosphor-svelte/lib/PauseIcon';
	import RepeatIcon from 'phosphor-svelte/lib/RepeatIcon';
	import RepeatOnceIcon from 'phosphor-svelte/lib/RepeatOnceIcon';
	import MonitorPlayIcon from 'phosphor-svelte/lib/MonitorPlayIcon';
	import MicrophoneStageIcon from 'phosphor-svelte/lib/MicrophoneStageIcon';
	import QueueIcon from 'phosphor-svelte/lib/QueueIcon';
	import DevicesIcon from 'phosphor-svelte/lib/DevicesIcon';
	import SpeakerSimpleXIcon from 'phosphor-svelte/lib/SpeakerSimpleXIcon';
	import SpeakerSimpleNoneIcon from 'phosphor-svelte/lib/SpeakerSimpleNoneIcon';
	import SpeakerSimpleLowIcon from 'phosphor-svelte/lib/SpeakerSimpleLowIcon';
	import SpeakerSimpleHighIcon from 'phosphor-svelte/lib/SpeakerSimpleHighIcon';
	import PictureInPictureIcon from 'phosphor-svelte/lib/PictureInPictureIcon';
	import CornersOutIcon from 'phosphor-svelte/lib/CornersOutIcon';
	import PlusCircleIcon from 'phosphor-svelte/lib/PlusCircleIcon';
	import CheckCircleIcon from 'phosphor-svelte/lib/CheckCircleIcon';
	import CaretUpIcon from 'phosphor-svelte/lib/CaretUpIcon';
	import ClockCounterClockwiseIcon from 'phosphor-svelte/lib/ClockCounterClockwiseIcon';
	import ClockClockwiseIcon from 'phosphor-svelte/lib/ClockClockwiseIcon';
	import SpeedButton from '$lib/nowplaying/SpeedButton.svelte';
	import VideoBox from '$lib/nowplaying/VideoBox.svelte'; // stream: video-pods
	import { showVideo } from '$lib/video.svelte';
	import { nowChapterIndex, nowChapters, trackNowChapters } from '$lib/chapters.svelte';
	import { chapterAt } from '$lib/chapters';
	import { settleOnRelease } from '$lib/scrub';
	// ── stream: smart2 ── Smart Shuffle: the shuffle button's third state.
	import SmartShuffleIcon from '$lib/smart/SmartShuffleIcon.svelte';
	import { cycleShuffle, initSmartShuffle, shuffleLabel, shuffleMode, smartEligible } from '$lib/smart/smartshuffle.svelte';

	const now = $derived(playback.now);
	// stream: video-pods. The player bar's own video thumbnail has the picture (see the cover).
	let barHolding = $state(false);
	const barVideo = $derived(showVideo() && barHolding);

	const currentSong = $derived.by((): SongItem | null => {
		const n = playback.now;
		if (!n) return null;
		const cur = playback.queue.items[playback.queue.currentIndex];
		if (cur?.video_id === n.videoId) return cur;
		return {
			video_id: n.videoId,
			title: n.title,
			artists: n.artists,
			artist_id: n.artistId,
			artist_runs: n.artistRuns,
			thumbnail: n.thumbnail
		};
	});
	const albumId = $derived(
		currentSong && !api.isLocalId(currentSong.video_id) ? currentSong.album_id : undefined
	);
	const liked = $derived(currentSong ? isLiked(currentSong) : false);
	const runs = $derived(now ? displayRuns(now.artistRuns, now.artists, now.artistId) : []);
	// stream: chapters — marks on the bar, the chapter under the cursor, the playing one's title.
	trackNowChapters();
	const chapters = $derived(now?.isEpisode ? nowChapters.list : []);
	const marks = $derived(chapters.map((c) => c.startSecs / (playback.speed || 1)));
	/** While dragging the progress bar: the playback position under the thumb. */
	let scrub = $state<number | null>(null);
	// While scrubbing, the chapter under the thumb (live, like the time beside it).
	const chapter = $derived(
		chapters.length ? chapters[scrub === null ? nowChapterIndex() : chapterAt(chapters, scrub)] : undefined
	);
	function progressTip(v: number) {
		const c = chapters[chapterAt(chapters, v * (playback.speed || 1))];
		return c ? `${c.title} · ${fmt(v)}` : fmt(v);
	}

	const shuffleOn = $derived(playback.queue.shuffle ?? false);
	initSmartShuffle();
	const sMode = $derived(shuffleMode(shuffleOn, playback.queue.sourceId));
	const repeat = $derived(playback.queue.repeat ?? 'off');

	// Progress
	const duration = $derived(playback.duration / (playback.speed || 1));
	const position = $derived((scrub ?? playback.position) / (playback.speed || 1));
	const fmt = (secs: number) => {
		if (!secs || secs < 0 || !Number.isFinite(secs)) return '0:00';
		const t = Math.floor(secs);
		const h = Math.floor(t / 3600);
		const m = Math.floor((t % 3600) / 60);
		const s = t % 60;
		return `${h ? `${h}:${String(m).padStart(2, '0')}` : m}:${String(s).padStart(2, '0')}`;
	};
	/** Release: the drag lands where the pointer let go, unless it let go slowly within a few
	 *  pixels of a chapter start, which it settles onto (`settleOnRelease`). Shown seconds in. */
	function seekCommit(v: number, drag?: { width: number; speed: number }) {
		const shown = drag ? settleOnRelease(marks, v, duration, drag.width, drag.speed) : v;
		const secs = shown * (playback.speed || 1);
		playback.position = secs;
		scrub = null;
		api.seek(secs);
	}

	// Volume
	const vol = $derived(playback.volume);
	const volIcon = $derived(
		vol === 0
			? SpeakerSimpleXIcon
			: vol < 34
				? SpeakerSimpleNoneIcon
				: vol < 67
					? SpeakerSimpleLowIcon
					: SpeakerSimpleHighIcon
	);
</script>

<!-- a11y: Spotify's player bar is an <aside> "Now playing bar". -->
<aside
	aria-label="Now playing bar"
	data-player-bar
	class="@container flex h-[72px] shrink-0 items-center justify-between bg-bg-app px-2 select-none"
	onwheel={(e) => {
		if ((e.target as HTMLElement).closest('[data-volume]')) wheelVolume(e);
	}}
>
	<!-- Left: now playing. a11y: a named region like Spotify's ("Now playing: X by Y"), with a
	     polite live region so a track change is announced. -->
	<div
		class="flex w-[30%] min-w-[180px] items-center"
		role="region"
		aria-label={now ? `Now playing: ${now.title} by ${now.artists}` : 'Now playing'}
	>
		<span class="visually-hidden" aria-live="polite" aria-atomic="true"
			>{now ? `Now playing: ${now.title} by ${now.artists}` : ''}</span
		>
		{#if now}
			<Tooltip text={shell.rightView === 'now-playing' ? 'Hide Now Playing view' : 'Show Now Playing view'}>
				{#snippet trigger(props)}
					<button
						{...props}
						type="button"
						class="group/cover relative h-14 shrink-0 overflow-hidden rounded-sm bg-bg-menu transition-[width] duration-200 {barVideo
							? 'w-[100px]'
							: 'w-14'}"
						aria-label="Now Playing view"
						onclick={() => toggleRight('now-playing')}
					>
						{#if now.thumbnail}
							<img src={thumb(now.thumbnail, 120)} alt="" class="size-full object-cover" />
						{/if}
						<!-- stream: video-pods. The live picture, 16:9, while no bigger view shows it (Spotify's
						     video podcasts). Ranked lowest, so it only gets the picture nobody else holds. -->
						{#if showVideo()}
							<VideoBox
								rank={0}
								fit="cover"
								bind:holding={barHolding}
								class="absolute inset-0 {barHolding ? '' : 'invisible'}"
							/>
						{/if}
						<span
							class="absolute top-1 right-1 grid size-6 place-items-center rounded-full bg-black/70 text-white opacity-0 transition-opacity group-hover/cover:opacity-100"
						>
							<Icon icon={CaretUpIcon} size={16} weight="bold" />
						</span>
					</button>
				{/snippet}
			</Tooltip>
			<div
				class="ml-3 min-w-0"
				role="presentation"
				oncontextmenu={(e) => currentSong && openTrackMenu(e, [currentSong])}
			>
				<!-- Truncated at rest; scrolls while hovered when it overflows (Spotify). -->
				<Marquee text={now.title} class="text-sm leading-[19px] text-white">
					{#if albumId}
						<a href={albumHref(albumId)} class="link">{now.title}</a>
					{:else}
						<span>{now.title}</span>
					{/if}
				</Marquee>
				<div class="truncate text-xs text-text-subdued">
					{#each runs as r, i (i)}
						{#if i > 0}<span>, </span>{/if}
						{#if r.id}
							<a href={artistHref(r.id)} class="link-sub">{r.text}</a>
						{:else}
							<span>{r.text}</span>
						{/if}
					{/each}
					{#if chapter}<span aria-hidden="true" class="mx-1">·</span><span data-testid="bar-chapter">{chapter.title}</span>{/if}
				</div>
			</div>
			<!-- Local files can be liked too, like Spotify (and like their track rows here already). -->
			{#if currentSong}
				<IconButton
					icon={liked ? CheckCircleIcon : PlusCircleIcon}
					fill={liked}
					active={liked}
					label={liked ? 'Add to playlist' : 'Add to Liked Songs'}
					class="ml-2"
					onclick={(e) => currentSong && openSavedIn(e, currentSong)}
				/>
			{/if}
		{/if}
	</div>

	<!-- Centre: controls + progress -->
	<div class="flex w-[40%] max-w-[722px] flex-col items-center gap-2">
		<div class="flex items-center gap-4">
			{#if now?.isEpisode}
				<!-- p-nowplaying hookup: podcast speed in shuffle's place, 15 s skips beside play. -->
				<SpeedButton />
				<IconButton icon={SkipBackIcon} fill label="Previous" onclick={() => api.prevTrack()} />
				<IconButton icon={ClockCounterClockwiseIcon} label="Back 15 seconds" onclick={() => skipBy(-SKIP_SECS)} />
			{:else}
			<!-- Spotify: 8px between shuffle and previous (and next and repeat), 16px around play. -->
			<div class="flex items-center gap-2">
			<IconButton
				icon={sMode === 'smart' ? SmartShuffleIcon : ShuffleIcon}
				label={shuffleLabel(sMode, smartEligible(playback.queue.sourceId))}
				active={shuffleOn}
				dot
				disabled={!now}
				onclick={() => cycleShuffle(shuffleOn, playback.queue.sourceId)}
			/>
			<IconButton icon={SkipBackIcon} fill label="Previous" disabled={!now} onclick={() => api.prevTrack()} />
			</div>
			{/if}
			<Tooltip text={playback.paused || !now ? 'Play' : 'Pause'}>
				{#snippet trigger(props)}
					<button
						{...props}
						type="button"
						aria-label={now?.loading ? 'Loading' : playback.paused || !now ? 'Play' : 'Pause'}
						disabled={!now}
						class="grid size-8 place-items-center rounded-full bg-white text-black transition-transform duration-50 hover:scale-[1.04] active:scale-100 active:opacity-70 disabled:opacity-30"
						onclick={() => api.togglePause()}
					>
						<!-- stream: instant. The tapped track shows at once; this spins until its audio starts. -->
						{#if now?.loading}
							<span class="size-4 animate-spin rounded-full border-2 border-black/25 border-t-black" data-loading></span>
						{:else}
							<Icon icon={playback.paused || !now ? PlayIcon : PauseIcon} size={16} fill />
						{/if}
					</button>
				{/snippet}
			</Tooltip>
			{#if now?.isEpisode}
				<IconButton icon={ClockClockwiseIcon} label="Forward 15 seconds" onclick={() => skipBy(SKIP_SECS)} />
			{/if}
			<div class="flex items-center gap-2">
			<IconButton icon={SkipForwardIcon} fill label="Next" disabled={!now} onclick={() => api.nextTrack()} />
			<IconButton
				icon={repeat === 'one' ? RepeatOnceIcon : RepeatIcon}
				label={repeat === 'off' ? 'Enable repeat' : repeat === 'all' ? 'Enable repeat one' : 'Disable repeat'}
				active={repeat !== 'off'}
				dot
				disabled={!now}
				onclick={() => cycleRepeat()}
			/>
			</div>
		</div>
		<div class="flex w-full items-center gap-2">
			<span class="min-w-10 text-right text-xs text-text-subdued tabular-nums">{fmt(position)}</span>
			{#if now?.live}
				<div class="flex-1 text-center text-xs font-bold text-white">LIVE</div>
			{:else}
				<Slider
					label="Change progress"
					value={position}
					max={duration || 1}
					step={5}
					tooltip={progressTip}
					valuetext={(v) => `${fmt(v)} of ${fmt(duration)}`}
					{marks}
					disabled={!now || !duration}
					oninput={(v) => (scrub = v * (playback.speed || 1))}
					onchange={seekCommit}
				/>
			{/if}
			<span class="min-w-10 text-left text-xs text-text-subdued tabular-nums">{fmt(duration)}</span>
		</div>
	</div>

	<!-- Right: extra controls -->
	<!-- left-2: Spotify's last button ends 8px from the window edge (the cover starts 16px in);
	     shifted, not padded, so the transport stays centred on the window. -->
	<!-- Narrow windows (Spotify's responsive bar): the volume slider and Miniplayer go first
	     (< 1000px), then Connect and Full screen (< 880px), so nothing runs into the transport. -->
	<div class="relative left-2 flex w-[30%] min-w-[180px] items-center justify-end">
		<IconButton
			icon={MonitorPlayIcon}
			tone="muted"
			label="Now playing view"
			active={shell.rightView === 'now-playing'}
			dot
			onclick={() => toggleRight('now-playing')}
		/>
		{#if !now || offersLyrics(now)}
			<IconButton
				icon={MicrophoneStageIcon}
				tone="muted"
				label="Lyrics"
				active={shell.lyricsOpen}
				dot
				disabled={!now}
				onclick={toggleLyrics}
			/>
		{/if}
		<IconButton
			icon={QueueIcon}
			tone="muted"
			label="Queue"
			active={shell.rightView === 'queue'}
			dot
			onclick={() => toggleRight('queue')}
		/>
		<IconButton
			icon={DevicesIcon}
			tone="muted"
			label="Connect to a device"
			active={shell.rightView === 'devices'}
			dot
			class="@max-[880px]:hidden"
			onclick={() => toggleRight('devices')}
		/>
		<div class="flex items-center" data-volume>
			<IconButton
				icon={volIcon}
				tone="muted"
				label={vol === 0 ? 'Unmute' : 'Mute'}
				onclick={toggleMute}
			/>
			<div class="w-[93px] @max-[1000px]:hidden">
				<Slider
					label="Change volume"
					valuetext={(v) => `${Math.round(v)}%`}
					value={vol}
					max={100}
					step={10}
					oninput={dragVolume}
					onchange={commitVolume}
				/>
			</div>
		</div>
		<IconButton
			icon={PictureInPictureIcon}
			tone="muted"
			label="Open Miniplayer"
			class="ml-2 @max-[1000px]:hidden"
			onclick={openMiniPlayer}
		/>
		<IconButton
			icon={CornersOutIcon}
			tone="muted"
			label="Enter Full screen"
			class="@max-[880px]:hidden"
			disabled={!now}
			onclick={() => (ui.theaterOpen = true)}
		/>
	</div>
</aside>
