// What the server writes into the page at startup (see apps/server/src/ui.rs).
// Every field is optional; a page opened from `npm run dev` gets the defaults.

export interface Settings {
  /** Where API calls go; empty means this page's own origin. */
  apiBase: string;
}

function read(): Settings {
  const text = document.getElementById('settings')?.textContent ?? '{}';
  try {
    const given = JSON.parse(text) as Partial<Settings>;
    return { apiBase: typeof given.apiBase === 'string' ? given.apiBase.replace(/\/$/, '') : '' };
  } catch {
    return { apiBase: '' };
  }
}

export const settings: Settings = read();
