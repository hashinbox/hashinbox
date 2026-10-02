// What the server writes into the page at startup (see apps/server/src/ui.rs).
// Every field is optional; a page opened from `npm run dev` gets the defaults.

export interface Settings {
  /** The name in the top bar. */
  brand: string;
  /** Where API calls go; empty means this page's own origin. */
  apiBase: string;
}

const DEFAULTS: Settings = { brand: 'tally', apiBase: '' };

function read(): Settings {
  const text = document.getElementById('settings')?.textContent ?? '{}';
  try {
    const given = JSON.parse(text) as Partial<Settings>;
    return {
      brand: typeof given.brand === 'string' && given.brand ? given.brand : DEFAULTS.brand,
      apiBase: typeof given.apiBase === 'string' ? given.apiBase.replace(/\/$/, '') : '',
    };
  } catch {
    return DEFAULTS;
  }
}

export const settings: Settings = read();
