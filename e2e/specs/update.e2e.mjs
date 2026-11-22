// The in-app updater, for real: the installed app checks its feed, offers the newer release,
// downloads it, verifies the signature, installs it and relaunches. The workflow (update job) has
// pointed the installed copy at a test feed carrying the newer release's real latest.json, and
// checks the result on disk afterwards (version of the installed binary, a running process).
import path from 'node:path';
import assert from 'node:assert/strict';

const OUT = path.resolve(process.env.E2E_OUT || 'out');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function toMain() {
	for (const h of await browser.getWindowHandles()) {
		await browser.switchToWindow(h);
		if (await browser.execute(() => window.__TAURI_INTERNALS__?.metadata?.currentWebview?.label === 'main').catch(() => false)) return;
	}
	throw new Error('no main webview');
}

describe('Updater', () => {
	it('finds the newer release, installs it, relaunches', async () => {
		const want = process.env.UPDATE_TO.replace(/^v/, '');
		await sleep(5000);
		await toMain();
		await $('[aria-label="Now playing bar"]').waitForDisplayed({ timeout: 60_000 });
		for (let i = 0; i < 4; i++) {
			const b = await $('//button[normalize-space(.)="Not now"]');
			if (await b.isDisplayed().catch(() => false)) await b.click();
			await sleep(600);
		}
		const from = await browser.execute(() => window.__TAURI_INTERNALS__.invoke('plugin:app|version'));
		await browser.execute(() => {
			const a = document.createElement('a');
			a.href = '/settings';
			document.body.appendChild(a);
			a.click();
		});
		const check = await $('//button[normalize-space(.)="Check for updates"]');
		await check.waitForExist({ timeout: 20_000 });
		await check.scrollIntoView({ block: 'center' });
		await check.click();
		const result = await $('[data-testid="update-result"]');
		await result.waitForDisplayed({ timeout: 60_000 });
		const text = await result.getText();
		await browser.saveScreenshot(path.join(OUT, 'shots', 'update-available.png'));
		assert.ok(text.includes(want), `offered ${want}: "${text}" (running ${from})`);
		const install = await $('//button[normalize-space(.)="Install"]');
		await install.click();
		console.log(`update ${from} → ${want}: install clicked`);
		// The app downloads, verifies, installs and restarts: this session ends with it.
		await sleep(60_000);
	});
});
