// OS-level checks the WebView cannot see: the system media session (SMTC / MPRIS), the tray,
// deep links through the OS URL handler, and the app's own log file. Runs in the WebdriverIO
// worker, on the same machine as the app.
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const IS_WIN = process.platform === 'win32';
export const IS_LINUX = process.platform === 'linux';

function ps(script, timeout = 60_000) {
	const r = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], {
		encoding: 'utf8',
		timeout
	});
	if (r.status !== 0) throw new Error(`powershell failed (${r.status}): ${r.stderr || r.stdout}`);
	return r.stdout.trim();
}

function sh(cmd, args, timeout = 30_000) {
	const r = spawnSync(cmd, args, { encoding: 'utf8', timeout });
	return { code: r.status, out: (r.stdout || '').trim(), err: (r.stderr || '').trim() };
}

// ── Windows: System Media Transport Controls, read the way the Windows media flyout does ──────
const SMTC_PRELUDE = `
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$asTask = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation\`1' })[0]
function Await($op, [Type]$t) { $m = $asTask.MakeGenericMethod($t); $task = $m.Invoke($null, @($op)); $task.Wait(-1) | Out-Null; $task.Result }
[void][Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType = WindowsRuntime]
$mgr = Await ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]::RequestAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager])
$s = $mgr.GetSessions() | Where-Object { $_.SourceAppUserModelId -match 'spotless' } | Select-Object -First 1
if (-not $s) { $s = $mgr.GetCurrentSession() }
`;

/** The OS media session: { app, title, artist, status } or null. */
export function mediaSession() {
	if (IS_WIN) {
		const out = ps(
			SMTC_PRELUDE +
				`if (-not $s) { 'null'; exit 0 }
$p = Await ($s.TryGetMediaPropertiesAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties])
@{ app = $s.SourceAppUserModelId; title = $p.Title; artist = $p.Artist; status = $s.GetPlaybackInfo().PlaybackStatus.ToString() } | ConvertTo-Json -Compress`
		);
		return out === 'null' || !out ? null : JSON.parse(out);
	}
	const names = sh('busctl', ['--user', 'list', '--no-legend']).out.split('\n');
	const bus = names.map((l) => l.split(/\s+/)[0]).find((n) => n?.startsWith('org.mpris.MediaPlayer2.'));
	if (!bus) return null;
	const get = (prop) =>
		sh('busctl', ['--user', '--json=short', 'get-property', bus, '/org/mpris/MediaPlayer2', 'org.mpris.MediaPlayer2.Player', prop]).out;
	const status = JSON.parse(get('PlaybackStatus') || '{"data":""}').data;
	const meta = JSON.parse(get('Metadata') || '{"data":{}}').data || {};
	return {
		app: bus,
		title: meta['xesam:title']?.data ?? '',
		artist: (meta['xesam:artist']?.data ?? []).join(', '),
		status
	};
}

/** Press play/pause the way a keyboard media key or the OS flyout does. */
export function mediaPlayPause() {
	if (IS_WIN) {
		ps(SMTC_PRELUDE + `if (-not $s) { throw 'no media session' }; $null = Await ($s.TryTogglePlayPauseAsync()) ([bool])`);
		return;
	}
	const bus = sh('busctl', ['--user', 'list', '--no-legend']).out.split('\n').map((l) => l.split(/\s+/)[0]).find((n) => n?.startsWith('org.mpris.MediaPlayer2.'));
	if (!bus) throw new Error('no MPRIS player on the session bus');
	const r = sh('busctl', ['--user', 'call', bus, '/org/mpris/MediaPlayer2', 'org.mpris.MediaPlayer2.Player', 'PlayPause']);
	if (r.code !== 0) throw new Error(`MPRIS PlayPause failed: ${r.err}`);
}

/** Open a URL through the OS handler, as a click in a browser or mail would. */
export function openUrl(url) {
	if (IS_WIN) {
		ps(`Start-Process '${url.replace(/'/g, "''")}'`);
		return;
	}
	// gio is what GNOME (and most desktops' file managers/browsers) open links with; xdg-open's
	// no-desktop fallback cannot run a quoted Exec= line, which is a test-box limitation.
	let r = sh('xdg-open', [url]);
	if (r.code !== 0) r = sh('gio', ['open', url]);
	if (r.code !== 0) throw new Error(`opening ${url} failed: ${r.err || r.out}`);
}

/** Which program the OS opens spotless:// links with (registry / xdg-mime). */
export function urlHandler() {
	if (IS_WIN) {
		try {
			return ps(`(Get-ItemProperty 'Registry::HKEY_CURRENT_USER\\Software\\Classes\\spotless\\shell\\open\\command' -ErrorAction Stop).'(default)'`);
		} catch {
			return ps(`(Get-ItemProperty 'Registry::HKEY_CLASSES_ROOT\\spotless\\shell\\open\\command' -ErrorAction SilentlyContinue).'(default)'`);
		}
	}
	return sh('xdg-mime', ['query', 'default', 'x-scheme-handler/spotless']).out;
}

// ── Tray ───────────────────────────────────────────────────────────────────────────────────────
/** Linux: the StatusNotifierItem our test watcher saw register (scripts/sni-watcher.py), with its
 *  menu labels read over com.canonical.dbusmenu. Windows: the notification-area button. */
export function trayInfo() {
	if (IS_WIN) {
		const out = ps(`
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button)
$all = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond)
$hit = @(); foreach ($e in $all) { if ($e.Current.Name -match 'Spotless') { $hit += $e.Current.Name } }
if (-not $hit) {
  # Windows 11 keeps new tray icons behind the "Show hidden icons" chevron: open it and look again.
  foreach ($e in $all) { if ($e.Current.Name -match 'hidden icons') { try { $e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Toggle() } catch { try { $e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke() } catch {} } } }
  Start-Sleep -Milliseconds 1500
  $all = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond)
  foreach ($e in $all) { if ($e.Current.Name -match 'Spotless') { $hit += $e.Current.Name } }
}
# What Explorer records for every tray icon it has shown (Windows 11): proof of registration.
$reg = Get-ChildItem 'HKCU:\Control Panel\NotifyIconSettings' -ErrorAction SilentlyContinue | Get-ItemProperty | Where-Object { $_.ExecutablePath -match 'Spotless' } | Select-Object -First 1 -ExpandProperty ExecutablePath
@{ hit = @($hit); reg = $reg } | ConvertTo-Json -Compress`);
		const o = out ? JSON.parse(out) : { hit: [], reg: null };
		const names = [].concat(o.hit ?? []).filter(Boolean);
		return { present: names.length > 0 || !!o.reg, names, registered: o.reg ?? null, clickable: names.length > 0 };
	}
	const file = path.join(os.tmpdir(), 'sni-items.txt');
	const items = fs.existsSync(file) ? fs.readFileSync(file, 'utf8').trim().split('\n').filter(Boolean) : [];
	const item = items.at(-1);
	if (!item) return { present: false, names: [] };
	const [svc, objPath = '/StatusNotifierItem'] = item.includes('/') ? [item.slice(0, item.indexOf('/')), item.slice(item.indexOf('/'))] : [item];
	const menuPath = JSON.parse(
		sh('busctl', ['--user', '--json=short', 'get-property', svc, objPath, 'org.kde.StatusNotifierItem', 'Menu']).out || '{"data":""}'
	).data;
	const layout = sh('busctl', ['--user', '--json=short', 'call', svc, menuPath, 'com.canonical.dbusmenu', 'GetLayout', '--', 'iias', '0', '-1', '0']).out;
	const labels = [...layout.matchAll(/"label":\{"type":"s","data":"([^"]*)"/g)].map((m) => m[1].replace(/_/g, ''));
	return { present: true, names: labels, svc, objPath, menuPath, layout };
}

/** Linux: click a tray menu item by label through dbusmenu (what the panel does on a click). */
export function trayClick(label) {
	const t = trayInfo();
	if (!t.present) throw new Error('no tray item');
	// Find the id that carries this label in the layout JSON.
	const re = new RegExp(`\\[(\\d+),\\{[^\\[]*?"label":\\{"type":"s","data":"_?${label.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}"`);
	const m = t.layout.match(re);
	if (!m) throw new Error(`no tray item "${label}" in ${t.names.join(', ')}`);
	const r = sh('busctl', ['--user', 'call', t.svc, t.menuPath, 'com.canonical.dbusmenu', 'Event', 'isvu', m[1], 'clicked', 's', '', '0']);
	if (r.code !== 0) throw new Error(`dbusmenu Event failed: ${r.err}`);
}

/** Windows: click the tray icon through UI Automation (Invoke, like a left click). */
export function trayInvokeWindows() {
	ps(`
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$root = [System.Windows.Automation.AutomationElement]::RootElement
$cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button)
$all = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond)
$e = $all | Where-Object { $_.Current.Name -match 'Spotless' } | Select-Object -First 1
if (-not $e) {
  foreach ($c in $all) { if ($c.Current.Name -match 'hidden icons') { try { $c.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Toggle() } catch { try { $c.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke() } catch {} } } }
  Start-Sleep -Milliseconds 1500
  $e = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond) | Where-Object { $_.Current.Name -match 'Spotless' } | Select-Object -First 1
}
if (-not $e) { throw 'tray button not found' }
$p = $e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern); $p.Invoke()`);
}

// ── The app's log ──────────────────────────────────────────────────────────────────────────────
export function logFiles() {
	const roots = IS_WIN
		? [process.env.APPDATA, process.env.LOCALAPPDATA].filter(Boolean)
		: [path.join(os.homedir(), '.local/share'), path.join(os.homedir(), '.config'), path.join(os.homedir(), '.cache')];
	const out = [];
	for (const r of roots) {
		for (const d of ['app.spotless.desktop', 'Spotless', 'spotless']) {
			const dir = path.join(r, d);
			if (!fs.existsSync(dir)) continue;
			for (const f of fs.readdirSync(dir)) if (f.endsWith('.log')) out.push(path.join(dir, f));
		}
	}
	return out;
}

export function logText() {
	return logFiles().map((f) => fs.readFileSync(f, 'utf8')).join('\n');
}

/** A short sine-wave WAV, for the local-files test (no codec or download needed). */
export function writeWav(file, secs = 30, hz = 440) {
	const rate = 44100;
	const n = rate * secs;
	const buf = Buffer.alloc(44 + n * 2);
	buf.write('RIFF', 0);
	buf.writeUInt32LE(36 + n * 2, 4);
	buf.write('WAVEfmt ', 8);
	buf.writeUInt32LE(16, 16);
	buf.writeUInt16LE(1, 20);
	buf.writeUInt16LE(1, 22);
	buf.writeUInt32LE(rate, 24);
	buf.writeUInt32LE(rate * 2, 28);
	buf.writeUInt16LE(2, 32);
	buf.writeUInt16LE(16, 34);
	buf.write('data', 36);
	buf.writeUInt32LE(n * 2, 40);
	for (let i = 0; i < n; i++) buf.writeInt16LE(Math.round(Math.sin((2 * Math.PI * hz * i) / rate) * 8000), 44 + i * 2);
	fs.mkdirSync(path.dirname(file), { recursive: true });
	fs.writeFileSync(file, buf);
}

export { execFileSync };

/** Start the app a second time, as a Start-menu/desktop-icon click would: the running copy should
 *  come to the front (single instance) and the new process exit. */
export async function launchAgain() {
	const { spawn } = await import('node:child_process');
	const env = { ...process.env };
	delete env.WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS;
	spawn(process.env.SPOTLESS_APP, [], { detached: true, stdio: 'ignore', env }).unref();
}
