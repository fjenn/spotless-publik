<script lang="ts">
	/**
	 * "Browse all" category tile (search-and-browse.md §5): ≈16:9, radius 8, solid colour,
	 * title 24/700 top-left (padding 16, right ≈30% so it wraps), artwork 45% of the width (its rotated
	 * box measures 143px at x+132, y+15 on a 238x134 tile, like Spotify),
	 * radius 4, rotated 25deg, bottom-right and clipped. Static on hover (cursor only).
	 */
	import { thumb } from '$lib/thumb';

	let {
		title,
		color,
		image,
		href,
		size = 'lg'
	}: { title: string; color: string; image?: string | null; href: string; size?: 'lg' | 'sm' } =
		$props();

	// a11y: YouTube's mood colours are often pastel (#ffe780 → white text at 1.2:1). Spotify's tiles
	// keep white titles readable, so darken (mix with black) until white text reaches 3:1 for the
	// 24px bold title (large text) or 4.5:1 for the small tile's 16px title.
	function lum(hex: string): number | null {
		const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
		if (!m) return null;
		const n = parseInt(m[1], 16);
		const c = [(n >> 16) & 255, (n >> 8) & 255, n & 255].map((v) => {
			const x = v / 255;
			return x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4;
		});
		return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
	}
	function readable(hex: string, ratio: number): string {
		const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
		if (!m) return hex;
		const n = parseInt(m[1], 16);
		let rgb = [(n >> 16) & 255, (n >> 8) & 255, n & 255];
		for (let i = 0; i < 20; i++) {
			const h = '#' + rgb.map((v) => Math.round(v).toString(16).padStart(2, '0')).join('');
			const l = lum(h);
			if (l === null || 1.05 / (l + 0.05) >= ratio) return h;
			rgb = rgb.map((v) => v * 0.92);
		}
		return '#' + rgb.map((v) => Math.round(v).toString(16).padStart(2, '0')).join('');
	}
	const bg = $derived(readable(color, size === 'lg' ? 3 : 4.5));
</script>

<a
	{href}
	class="relative block aspect-[16/9] w-full overflow-hidden rounded-lg [container-type:inline-size]"
	style="background-color:{bg}"
	draggable="false"
>
	<span
		class="absolute top-0 left-0 block max-w-[70%] p-4 font-title font-bold text-white [hyphens:manual] [overflow-wrap:normal] [word-break:normal] {size ===
		'lg'
			? 'text-xl leading-7 tile-title-lg'
			: 'text-base leading-5 tile-title'}">{title}</span
	>
	{#if image}
		<img
			src={thumb(image, 300)}
			alt=""
			loading="lazy"
			draggable="false"
			class="absolute top-[24.6%] left-[63%] aspect-square w-[45%] rotate-[25deg] rounded-sm object-cover shadow-[0_2px_4px_rgb(0_0_0/0.2)]"
		/>
	{/if}
</a>

<style>
	/* Never split a word ("Podcast/s", "Audiobo/oks" on Linux, whose fallback font is wider).
	   Instead the title shrinks with the tile when a ~10-letter word would not fit the label's
	   width (70% of the tile minus the 2rem padding); at normal tile widths the min() keeps the
	   design size, so the Mac look is unchanged. */
	.tile-title {
		font-size: min(1rem, calc((70cqi - 2rem) / 6.2));
	}
	.tile-title-lg {
		font-size: min(1.25rem, calc((70cqi - 2rem) / 6.2));
	}
</style>
