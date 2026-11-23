// WebdriverIO against the INSTALLED Spotless app.
//   Linux:   tauri-driver (Tauri's WebDriver server) → WebKitWebDriver drives WebKitGTK.
//   Windows: the app is started with WebView2's remote-debugging port open, and msedgedriver
//            attaches to it (Microsoft's documented WebView2 route). tauri-driver's own launch mode
//            never saw the DevTools port on the runner (msedgedriver: "DevToolsActivePort file
//            doesn't exist"), so the app is launched here and attached to instead.
//   SPOTLESS_APP   the installed binary (Spotless.exe / the AppImage / /usr/bin/spotless)
//   E2E_OUT        where screenshots and results go
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const APP = process.env.SPOTLESS_APP;
const OUT = path.resolve(process.env.E2E_OUT || 'out');
const RESULTS = path.join(OUT, 'results.jsonl');
const WIN = process.platform === 'win32';
const CDP_PORT = 9222;
let driver;
let app;

async function waitFor(url, ms) {
	const end = Date.now() + ms;
	while (Date.now() < end) {
		try {
			const r = await fetch(url);
			if (r.ok) return await r.text();
		} catch {}
		await new Promise((r) => setTimeout(r, 1000));
	}
	return null;
}

const capabilities = WIN
	? {
			browserName: 'webview2',
			'wdio:enforceWebDriverClassic': true,
			'ms:edgeOptions': { debuggerAddress: `127.0.0.1:${CDP_PORT}` }
		}
	: {
			'wdio:enforceWebDriverClassic': true,
			'tauri:options': { application: APP }
		};

export const config = {
	runner: 'local',
	specs: [process.env.E2E_SPEC || './specs/parity.e2e.mjs'],
	maxInstances: 1,
	hostname: '127.0.0.1',
	port: 4444,
	capabilities: [{ maxInstances: 1, ...capabilities }],
	logLevel: 'warn',
	bail: 0,
	waitforTimeout: 20_000,
	connectionRetryTimeout: 180_000,
	connectionRetryCount: 1,
	framework: 'mocha',
	reporters: ['spec'],
	mochaOpts: { ui: 'bdd', timeout: 300_000 },

	async onPrepare() {
		if (!APP || !fs.existsSync(APP)) throw new Error(`SPOTLESS_APP not found: ${APP}`);
		fs.mkdirSync(path.join(OUT, 'shots'), { recursive: true });
		fs.writeFileSync(RESULTS, '');
		if (WIN) {
			driver = spawn(process.env.NATIVE_DRIVER, [`--port=4444`, '--verbose', `--log-path=${path.join(OUT, 'msedgedriver.log')}`], {
				stdio: 'inherit'
			});
			app = spawn(APP, [], {
				stdio: 'ignore',
				detached: true,
				env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}` }
			});
			const v = await waitFor(`http://127.0.0.1:${CDP_PORT}/json/version`, 120_000);
			if (!v) throw new Error('the app never opened the WebView2 DevTools port');
			console.log('WebView2 DevTools:', v);
			// Give the main window (built after the core boots) time to exist.
			const end = Date.now() + 90_000;
			while (Date.now() < end) {
				const list = (await waitFor(`http://127.0.0.1:${CDP_PORT}/json/list`, 5000)) ?? '[]';
				if (/tauri\.localhost/.test(list)) {
					console.log('targets:', list);
					break;
				}
				await new Promise((r) => setTimeout(r, 1000));
			}
			return;
		}
		const bin = path.join(os.homedir(), '.cargo', 'bin', 'tauri-driver');
		driver = spawn(bin, [], { stdio: [null, process.stdout, process.stderr] });
		await new Promise((r) => setTimeout(r, 3000));
	},

	afterTest(test, _ctx, { passed, error, duration }) {
		if (!passed && !error) return; // skipped: the test recorded its own row
		fs.appendFileSync(
			RESULTS,
			JSON.stringify({
				suite: test.parent,
				title: test.title,
				passed,
				ms: duration,
				error: error ? String(error.message || error).split('\n')[0].slice(0, 300) : null
			}) + '\n'
		);
	},

	onComplete() {
		driver?.kill();
		try {
			if (app?.pid) process.kill(app.pid);
		} catch {}
		const rows = fs.existsSync(RESULTS)
			? fs.readFileSync(RESULTS, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l))
			: [];
		const notesFile = path.join(OUT, 'notes.jsonl');
		const notes = fs.existsSync(notesFile)
			? Object.fromEntries(
					fs.readFileSync(notesFile, 'utf8').trim().split('\n').filter(Boolean).map((l) => {
						const n = JSON.parse(l);
						return [n.title, n.note];
					})
				)
			: {};
		const md = [
			`## Spotless E2E (${process.platform}) — ${rows.filter((r) => r.passed === true).length}/${rows.length} passed, ${rows.filter((r) => r.passed === 'skip').length} by proxy`,
			'',
			'| area | feature | result | notes |',
			'|---|---|---|---|',
			...rows.map(
				(r) =>
					`| ${r.suite} | ${r.title} | ${r.passed === 'skip' ? 'proxy / environment' : r.passed ? 'pass' : '**FAIL**'} | ${(r.error || notes[r.title] || '').replace(/\|/g, '\\|')} |`
			)
		].join('\n');
		fs.writeFileSync(path.join(OUT, 'results.md'), md + '\n');
		if (process.env.GITHUB_STEP_SUMMARY) fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, md + '\n');
	}
};
