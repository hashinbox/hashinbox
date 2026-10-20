import AppBar from '@mui/material/AppBar';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Link from '@mui/material/Link';
import Stack from '@mui/material/Stack';
import Tab from '@mui/material/Tab';
import Tabs from '@mui/material/Tabs';
import TextField from '@mui/material/TextField';
import Toolbar from '@mui/material/Toolbar';
import Typography from '@mui/material/Typography';
import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  ApiError,
  call,
  client,
  health,
  setToken,
  whenUnauthorized,
  type Listing,
  type Wallet,
} from '@/api/client';
import { Networks, type Line } from '@/_components/networks';
import { Toast, type Message } from '@/_components/toast';
import { Docs } from '@/pages/docs/page';
import { Networks as Configured } from '@/pages/networks/page';
import { Triggers } from '@/pages/triggers/page';
import { Wallets } from '@/pages/wallets/page';
import { settings } from '@/settings';
import { display } from '@/theme';

type View = 'wallets' | 'triggers' | 'networks' | 'docs';
/** Whether the server wants a token: not asked yet, no, or yes. */
type Door = 'unknown' | 'open' | 'locked';
const TOKEN_KEY = 'tally.token';
const VIEWS: View[] = ['wallets', 'triggers', 'networks', 'docs'];

/** The tab named in the address, so a reload or a shared link lands on it. */
function linked(): View {
  const named = window.location.hash.slice(1).split('/')[0];
  return VIEWS.find((v) => v === named) ?? 'wallets';
}
const BEAT_MS = 4000;

/** Everything a page needs to talk back to the shell. */
export interface Shell {
  listings: Listing[];
  notify: (text: string, bad?: boolean) => void;
  fail: (error: unknown) => void;
}

function stored(): string | null {
  try {
    return sessionStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

export function Layout() {
  const [token, setSession] = useState<string | null>(stored);
  const [door, setDoor] = useState<Door>('unknown');
  const [view, setView] = useState<View>(linked);
  const [listings, setListings] = useState<Listing[]>([]);
  const [wallets, setWallets] = useState<Wallet[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [tags, setTags] = useState<Map<string, string[]>>(new Map());
  const [tagged, setTagged] = useState(0);
  const [lines, setLines] = useState<Line[]>([]);
  const [ready, setReady] = useState<boolean | null>(null);
  const [toast, setToast] = useState<Message | null>(null);

  const notify = useCallback((text: string, bad = false) => {
    setToast({ text, bad });
  }, []);
  const fail = useCallback(
    (error: unknown) => {
      if (error instanceof ApiError && error.status === 401) return;
      notify(error instanceof Error ? error.message : String(error), true);
    },
    [notify],
  );

  const lock = useCallback(() => {
    setToken(null);
    setSession(null);
    setDoor('locked');
    try {
      sessionStorage.removeItem(TOKEN_KEY);
    } catch {
      /* private window */
    }
  }, []);

  useEffect(() => {
    whenUnauthorized(() => {
      lock();
      notify('The token was refused.', true);
    });
  }, [lock, notify]);

  // Ask, with whatever token this tab holds, whether the door is open.
  useEffect(() => {
    setToken(token);
    let live = true;
    call(client.GET('/v1/assets'))
      .then(() => {
        if (!live) return;
        setDoor('open');
      })
      .catch((error: unknown) => {
        if (!live) return;
        if (error instanceof ApiError && error.status === 401) setDoor('locked');
        else fail(error);
      });
    return () => {
      live = false;
    };
  }, [token, fail]);

  // The wallets and the assets they can be made in.
  useEffect(() => {
    if (door !== 'open') return;
    let live = true;
    Promise.all([
      call(client.GET('/v1/assets')),
      call(client.GET('/v1/wallets')),
      call(client.GET('/v1/tags')),
    ])
      .then(([assets, list, all]) => {
        if (!live) return;
        setListings(assets.assets);
        setWallets(list.wallets);
        const byWallet = new Map<string, string[]>();
        for (const t of all.tags)
          byWallet.set(t.wallet_id, [...(byWallet.get(t.wallet_id) ?? []), t.tag]);
        setTags(byWallet);
        setSelected((current) => current ?? list.wallets[0]?.id ?? null);
      })
      .catch((error: unknown) => {
        if (live) fail(error);
      });
    return () => {
      live = false;
    };
  }, [door, token, view, tagged, fail]);

  useEffect(() => {
    const navigate = () => setView(linked());
    window.addEventListener('hashchange', navigate);
    window.addEventListener('popstate', navigate);
    return () => {
      window.removeEventListener('hashchange', navigate);
      window.removeEventListener('popstate', navigate);
    };
  }, []);

  // The ticker reads the public health report and needs no token.
  useEffect(() => {
    let live = true;
    const beat = () =>
      health()
        .then((report) => {
          if (!live) return;
          setReady(report.ready);
          setLines(
            report.networks.map((n) => ({
              name: `${n.network.chain}/${n.network.name}`,
              state: n.indexing.error ? 'retrying' : 'ready',
              height: n.indexing.last,
              tip: n.indexing.tip,
              error: n.indexing.error,
            })),
          );
        })
        .catch(() => {
          if (!live) return;
          setReady(false);
          setLines([]);
        });
    void beat();
    const timer = window.setInterval(() => void beat(), BEAT_MS);
    return () => {
      live = false;
      window.clearInterval(timer);
    };
  }, []);

  const unlock = (value: string) => {
    try {
      sessionStorage.setItem(TOKEN_KEY, value);
    } catch {
      /* private window */
    }
    setSession(value);
  };

  // Added, or watched in more assets: replaces the listed one with the same id.
  const added = (wallet: Wallet) => {
    setWallets((list) => [...list.filter((w) => w.id !== wallet.id), wallet]);
    setSelected(wallet.id);
    setView('wallets');
    setTagged((n) => n + 1);
  };

  useEffect(() => {
    if (linked() !== view) window.history.replaceState(null, '', `#${view}`);
  }, [view]);

  const shell = useMemo<Shell>(() => ({ listings, notify, fail }), [listings, notify, fail]);
  const open = door === 'open';

  return (
    <>
      <AppBar>
        <Toolbar sx={{ gap: 4, flexWrap: 'wrap', minHeight: 60 }}>
          <Link
            href="/"
            underline="none"
            color="text.primary"
            sx={{ fontFamily: display, fontWeight: 600, fontSize: 17, letterSpacing: '-.01em' }}
          >
            {settings.brand}
          </Link>
          {door !== 'unknown' && (
            <Tabs
              value={open || view === 'docs' ? view : false}
              onChange={(_, next: View) => setView(next)}
              aria-label="Views"
            >
              {open && <Tab value="wallets" label="Wallets" />}
              {open && <Tab value="triggers" label="Triggers" />}
              {open && <Tab value="networks" label="Networks" />}
              <Tab value="docs" label="Docs" />
            </Tabs>
          )}
          <Box sx={{ flexGrow: 1 }} />
          <Networks lines={lines} ready={ready} />
          {token && open && (
            <Button color="inherit" onClick={lock}>
              Lock
            </Button>
          )}
        </Toolbar>
      </AppBar>

      {open && (
        <Box component="main">
          {view === 'wallets' && (
            <Wallets
              shell={shell}
              wallets={wallets}
              selected={selected}
              onSelect={setSelected}
              onAdded={added}
              tags={tags}
              onTagged={() => setTagged((n) => n + 1)}
            />
          )}
          {view === 'triggers' && <Triggers shell={shell} wallets={wallets} />}
          {view === 'networks' && <Configured shell={shell} />}
          {view === 'docs' && <Docs shell={shell} secured={token !== null} />}
        </Box>
      )}
      {door === 'locked' && view === 'docs' && (
        <Box component="main">
          <Docs shell={shell} secured />
        </Box>
      )}
      {door === 'locked' && view !== 'docs' && (
        <Box component="main" sx={{ px: 3, py: 10, maxWidth: 560, mx: 'auto' }}>
          <Typography variant="overline" component="p">
            Authorization required
          </Typography>
          <Typography variant="h2" component="h1" sx={{ mt: 1 }}>
            This server asks for a token.
          </Typography>
          <Typography variant="body2" color="text.secondary" sx={{ mt: 1.5 }}>
            It stays in this tab only and is sent as the Authorization header.
          </Typography>
          <Box sx={{ mt: 4 }}>
            <Session onUnlock={unlock} />
          </Box>
        </Box>
      )}

      <Toast message={toast} onClose={() => setToast(null)} />
    </>
  );
}

function Session({ onUnlock }: { onUnlock: (token: string) => void }) {
  const [value, setValue] = useState('');
  return (
    <Stack
      component="form"
      direction="row"
      spacing={1.5}
      sx={{ alignItems: 'flex-end' }}
      autoComplete="off"
      onSubmit={(event) => {
        event.preventDefault();
        const token = value.trim();
        if (token) onUnlock(token);
      }}
    >
      <TextField
        type="password"
        placeholder="bearer token"
        value={value}
        onChange={(e) => setValue(e.target.value)}
        slotProps={{ htmlInput: { 'aria-label': 'Bearer token' } }}
        autoFocus
        required
      />
      <Button type="submit" variant="contained">
        Unlock
      </Button>
    </Stack>
  );
}
