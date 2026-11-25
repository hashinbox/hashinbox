import AppBar from '@mui/material/AppBar';
import AccountBalanceWallet from '@mui/icons-material/AccountBalanceWallet';
import Bolt from '@mui/icons-material/Bolt';
import Close from '@mui/icons-material/Close';
import DashboardIcon from '@mui/icons-material/Dashboard';
import Hub from '@mui/icons-material/Hub';
import LockIcon from '@mui/icons-material/Lock';
import MenuIcon from '@mui/icons-material/Menu';
import MenuBook from '@mui/icons-material/MenuBook';
import Notes from '@mui/icons-material/Notes';
import ShowChart from '@mui/icons-material/ShowChart';
import WorkHistory from '@mui/icons-material/WorkHistory';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Drawer from '@mui/material/Drawer';
import IconButton from '@mui/material/IconButton';
import Link from '@mui/material/Link';
import List from '@mui/material/List';
import ListItemButton from '@mui/material/ListItemButton';
import ListItemIcon from '@mui/material/ListItemIcon';
import ListItemText from '@mui/material/ListItemText';
import Stack from '@mui/material/Stack';
import Tab from '@mui/material/Tab';
import Tabs from '@mui/material/Tabs';
import TextField from '@mui/material/TextField';
import Toolbar from '@mui/material/Toolbar';
import Typography from '@mui/material/Typography';
import { useTheme } from '@mui/material/styles';
import useMediaQuery from '@mui/material/useMediaQuery';
import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import {
  ApiError,
  call,
  client,
  setToken,
  whenUnauthorized,
  type Listing,
  type Wallet,
} from '@/api/client';
import { Toast, type Message } from '@/_components/toast';
import { LogsDrawer } from '@/_components/logs/drawer';
import { logSource } from '@/api/logs';
import { Docs } from '@/pages/docs/page';
import { Explorers, type Explorer } from '@/_components/explorers';
import { Dashboard } from '@/pages/dashboard/page';
import { Jobs } from '@/pages/jobs/page';
import { Logs } from '@/pages/logs/page';
import { Markets } from '@/pages/markets/page';
import { Networks as Configured } from '@/pages/networks/page';
import { Triggers } from '@/pages/triggers/page';
import { Wallets } from '@/pages/wallets/page';
import { go, segments } from '@/route';
import { settings } from '@/settings';
import { display } from '@/theme';

type View =
  'dashboard' | 'wallets' | 'triggers' | 'networks' | 'markets' | 'jobs' | 'logs' | 'docs';
/** Whether the server wants a token: not asked yet, no, or yes. */
type Door = 'unknown' | 'open' | 'locked';
const TOKEN_KEY = 'tally.token';
const VIEWS: View[] = [
  'dashboard',
  'wallets',
  'triggers',
  'networks',
  'markets',
  'jobs',
  'logs',
  'docs',
];

/** The tab named in the address, so a reload or a shared link lands on it. */
function linked(): View {
  const named = segments()[0];
  return VIEWS.find((v) => v === named) ?? 'dashboard';
}

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

/** Every wallet, page by page. */
async function allWallets(): Promise<Wallet[]> {
  const wallets: Wallet[] = [];
  let cursor: string | undefined;
  do {
    const query = { limit: 500, ...(cursor !== undefined && { cursor }) };
    const page = await call(client.GET('/v1/wallets', { params: { query } }));
    wallets.push(...page.wallets);
    cursor = page.next_cursor ?? undefined;
  } while (cursor !== undefined);
  return wallets;
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
  const [explorers, setExplorers] = useState<Map<string, Explorer>>(new Map());
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
    Promise.all([call(client.GET('/v1/assets')), allWallets(), call(client.GET('/v1/networks'))])
      .then(([assets, list, configured]) => {
        if (!live) return;
        const byNetwork = new Map<string, Explorer>();
        for (const n of configured.networks)
          if (n.explorer_address && n.explorer_transaction)
            byNetwork.set(`${n.chain}/${n.network}`, {
              address: n.explorer_address,
              transaction: n.explorer_transaction,
            });
        setExplorers(byNetwork);
        setListings(assets.assets);
        setWallets(list);
        setTags(new Map(list.map((w) => [w.id, w.tags])));
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
    window.addEventListener('popstate', navigate);
    return () => window.removeEventListener('popstate', navigate);
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
    go(['wallets']);
    setTagged((n) => n + 1);
  };

  useEffect(() => {
    if (linked() !== view || segments().length === 0) go([view], true);
  }, [view]);

  const shell = useMemo<Shell>(() => ({ listings, notify, fail }), [listings, notify, fail]);
  const open = door === 'open';
  const theme = useTheme();
  // Narrow screens fold the views into a menu that opens from the top.
  const wide = useMediaQuery(theme.breakpoints.up('md'));
  const [menu, setMenu] = useState(false);
  const views: { view: View; label: string; icon: ReactNode }[] = [
    ...(open
      ? [
          {
            view: 'dashboard' as const,
            label: 'Dashboard',
            icon: <DashboardIcon fontSize="small" />,
          },
          {
            view: 'wallets' as const,
            label: 'Wallets',
            icon: <AccountBalanceWallet fontSize="small" />,
          },
          { view: 'triggers' as const, label: 'Triggers', icon: <Bolt fontSize="small" /> },
          { view: 'networks' as const, label: 'Networks', icon: <Hub fontSize="small" /> },
          { view: 'markets' as const, label: 'Markets', icon: <ShowChart fontSize="small" /> },
          { view: 'jobs' as const, label: 'Jobs', icon: <WorkHistory fontSize="small" /> },
          { view: 'logs' as const, label: 'Logs', icon: <Notes fontSize="small" /> },
        ]
      : []),
    { view: 'docs', label: 'Docs', icon: <MenuBook fontSize="small" /> },
  ];

  return (
    // The page is at least the screen's height; `main` fills what the bar leaves.
    <Box sx={{ display: 'flex', flexDirection: 'column', minHeight: '100dvh' }}>
      <AppBar>
        <Toolbar sx={{ gap: wide ? 4 : 1, flexWrap: 'wrap', minHeight: 60 }}>
          {door !== 'unknown' && !wide && (
            <IconButton edge="start" aria-label="Menu" onClick={() => setMenu(true)}>
              <MenuIcon />
            </IconButton>
          )}
          <Link
            href="/"
            underline="none"
            color="text.primary"
            sx={{ fontFamily: display, fontWeight: 600, fontSize: 17, letterSpacing: '-.01em' }}
          >
            {settings.brand}
          </Link>
          {door !== 'unknown' && wide && (
            <Tabs
              value={open || view === 'docs' ? view : false}
              onChange={(_, next: View) => go([next])}
              aria-label="Views"
            >
              {views.map((v) => (
                <Tab key={v.view} value={v.view} label={v.label} />
              ))}
            </Tabs>
          )}
          <Box sx={{ flexGrow: 1 }} />
          {token && open && wide && (
            <Button color="inherit" onClick={lock}>
              Lock
            </Button>
          )}
        </Toolbar>
      </AppBar>
      <Drawer
        anchor="left"
        open={menu && !wide}
        onClose={() => setMenu(false)}
        slotProps={{ paper: { sx: { width: 272, display: 'flex', flexDirection: 'column' } } }}
      >
        <Stack
          direction="row"
          sx={{
            alignItems: 'center',
            px: 2.5,
            minHeight: 60,
            borderBottom: 1,
            borderColor: 'divider',
          }}
        >
          <Typography
            sx={{
              flexGrow: 1,
              fontFamily: display,
              fontWeight: 600,
              fontSize: 17,
              letterSpacing: '-.01em',
            }}
          >
            {settings.brand}
          </Typography>
          <IconButton size="small" aria-label="Close menu" onClick={() => setMenu(false)}>
            <Close fontSize="small" />
          </IconButton>
        </Stack>
        <List aria-label="Views" sx={{ px: 1.5, py: 1.5, flexGrow: 1 }}>
          {views.map((v) => (
            <ListItemButton
              key={v.view}
              selected={v.view === view}
              onClick={() => {
                setMenu(false);
                go([v.view]);
              }}
              sx={{ borderRadius: 2, mb: 0.5, py: 1.1 }}
            >
              <ListItemIcon sx={{ minWidth: 38 }}>{v.icon}</ListItemIcon>
              <ListItemText primary={v.label} />
            </ListItemButton>
          ))}
        </List>
        {token && open && (
          <List sx={{ px: 1.5, py: 1.5, borderTop: 1, borderColor: 'divider' }}>
            <ListItemButton
              onClick={() => {
                setMenu(false);
                lock();
              }}
              sx={{ borderRadius: 2, py: 1.1 }}
            >
              <ListItemIcon sx={{ minWidth: 38 }}>
                <LockIcon fontSize="small" />
              </ListItemIcon>
              <ListItemText primary="Lock" />
            </ListItemButton>
          </List>
        )}
      </Drawer>

      {open && (
        <Explorers.Provider value={explorers}>
          <Box component="main" sx={{ flexGrow: 1, display: 'flex', flexDirection: 'column' }}>
            {view === 'dashboard' && <Dashboard shell={shell} />}
            {view === 'wallets' && (
              <Wallets
                shell={shell}
                wallets={wallets}
                selected={selected}
                onSelect={setSelected}
                onClose={() => setSelected(null)}
                onAdded={added}
                tags={tags}
                onTagged={() => setTagged((n) => n + 1)}
              />
            )}
            {view === 'triggers' && <Triggers shell={shell} wallets={wallets} />}
            {view === 'networks' && <Configured shell={shell} />}
            {view === 'markets' && <Markets shell={shell} />}
            {view === 'jobs' && <Jobs shell={shell} />}
            {view === 'logs' && <Logs shell={shell} />}
            {view === 'docs' && <Docs shell={shell} secured={token !== null} />}
          </Box>
          <LogsDrawer shell={shell} source={logSource} />
        </Explorers.Provider>
      )}
      {door === 'locked' && view === 'docs' && (
        <Box component="main" sx={{ flexGrow: 1, display: 'flex', flexDirection: 'column' }}>
          <Docs shell={shell} secured />
        </Box>
      )}
      {door === 'locked' && view !== 'docs' && (
        <Box component="main" sx={{ flexGrow: 1, px: 3, py: 10, maxWidth: 560, mx: 'auto' }}>
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
    </Box>
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
