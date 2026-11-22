// Words that name this computer or its file manager, so Windows and Linux never read "this Mac" or
// "Show in Finder". Same detection as `IS_MAC` in shortcuts.ts.
import { browser } from '$app/environment';

const ua = browser ? navigator.userAgent : '';
const IS_MAC_UA =
	browser &&
	((navigator as unknown as { userAgentData?: { platform?: string } }).userAgentData?.platform === 'macOS' || /Mac/.test(ua));
const IS_WIN_UA = browser && /Windows/.test(ua);

/** "this Mac" / "this PC" / "this computer". */
export const THIS_DEVICE = IS_MAC_UA ? 'this Mac' : IS_WIN_UA ? 'this PC' : 'this computer';
/** "This Mac" / "This PC" / "This computer": the same, starting a sentence or a label. */
export const THIS_DEVICE_TITLE = THIS_DEVICE[0].toUpperCase() + THIS_DEVICE.slice(1);
/** "Show in Finder" / "Show in File Explorer" / "Show in folder". */
export const SHOW_IN_FILES = IS_MAC_UA ? 'Show in Finder' : IS_WIN_UA ? 'Show in File Explorer' : 'Show in folder';
