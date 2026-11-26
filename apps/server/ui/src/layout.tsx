import AppBar from '@mui/material/AppBar';
import Avatar from '@mui/material/Avatar';
import AccountBalanceWallet from '@mui/icons-material/AccountBalanceWallet';
import Bolt from '@mui/icons-material/Bolt';
import Close from '@mui/icons-material/Close';
import DashboardIcon from '@mui/icons-material/Dashboard';
import Hub from '@mui/icons-material/Hub';
import MenuIcon from '@mui/icons-material/Menu';
import MenuBook from '@mui/icons-material/MenuBook';
import Notes from '@mui/icons-material/Notes';
import People from '@mui/icons-material/People';
import ShowChart from '@mui/icons-material/ShowChart';
import WorkHistory from '@mui/icons-material/WorkHistory';
import Box from '@mui/material/Box';
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
import Toolbar from '@mui/material/Toolbar';
import Typography from '@mui/material/Typography';
import { useTheme } from '@mui/material/styles';
import useMediaQuery from '@mui/material/useMediaQuery';
import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import {
  assetsList,
  networksList,
  ownUserGet,
  recoveryCreateOperation,
  Session,
  usersListOperation,
  versionGet,
  walletsList,
  type Listing,
  type User,
  type UserRead,
  type Wallet,
} from '@hashinbox/client';
import { ApiError, api, whenUnauthorized } from '@/api/client';
import { Toast, type Message } from '@/_components/toast';
import { LogsDrawer } from '@/_components/logs/drawer';
import { logSource } from '@/api/logs';
import { Docs } from '@/pages/docs/page';
import { Explorers, type Explorer } from '@/_components/explorers';
import { Dashboard } from '@/pages/dashboard/page';
import { Jobs } from '@/pages/jobs/page';
import { Login } from '@/pages/login/page';
import { Logs } from '@/pages/logs/page';
import { Markets } from '@/pages/markets/page';
import { Networks as Configured } from '@/pages/networks/page';
import { Setup } from '@/pages/setup/page';
import { Triggers } from '@/pages/triggers/page';
import { RecoveryBanner } from '@/pages/users/_components/recovery';
import { Me } from '@/pages/users/me';
import { Users } from '@/pages/users/page';
import { Wallets } from '@/pages/wallets/page';
import { go, segments } from '@/route';
import { display } from '@/theme';

type View =
  | 'dashboard'
  | 'wallets'
  | 'triggers'
  | 'networks'
  | 'markets'
  | 'jobs'
  | 'logs'
  | 'users'
  | 'user'
  | 'docs'
  | 'login'
  | 'setup';
/**
 * Where setup stands (`GET /v1/version`): not asked yet, no user (fresh
 * install, until setup is done), or done.
 */
type Root = 'unknown' | 'missing' | 'present';
const VIEWS: View[] = [
  'dashboard',
  'wallets',
  'triggers',
  'networks',
  'markets',
  'jobs',
  'logs',
  'users',
  'user',
  'docs',
  'login',
  'setup',
];

/** The name shown when the server's `instance` setting is unset. */
const NAME = 'hashinbox';

/**
 * The view a state may show: setup on a fresh install, login without a
 * session, never login or setup after.
 */
function allowed(view: View, root: Root, token: string | null): View {
  if (root === 'missing') return 'setup';
  if (token === null) return 'login';
  return view === 'login' || view === 'setup' ? 'dashboard' : view;
}

/** The tab named in the address, so a reload or a shared link lands on it. */
function linked(): View {
  const named = segments()[0];
  return VIEWS.find((v) => v === named) ?? 'dashboard';
}

/** "Richard Huttar" → "RH", one word → its first letter, no name → the email's. */
function initials(user: User): string {
  const words = user.name.trim().split(/\s+/).filter(Boolean);
  const first = words[0]?.[0] ?? user.email[0] ?? '?';
  const last = words.length > 1 ? (words[words.length - 1]?.[0] ?? '') : '';
  return (first + last).toUpperCase();
}

/** Everything a page needs to talk back to the shell. */
export interface Shell {
  listings: Listing[];
  notify: (text: string, bad?: boolean) => void;
  fail: (error: unknown) => void;
}

/** Every wallet, page by page. */
async function allWallets(): Promise<Wallet[]> {
  const wallets: Wallet[] = [];
  let cursor: string | undefined;
  do {
    const page = await walletsList(api, { limit: 500, ...(cursor !== undefined && { cursor }) });
    wallets.push(...page.wallets);
    cursor = page.next_cursor ?? undefined;
  } while (cursor !== undefined);
  return wallets;
}

export function Layout() {
  const [token, setSession] = useState<string | null>(() => api.session?.token ?? null);
  const [root, setRoot] = useState<Root>('unknown');
  const [name, setName] = useState(NAME);
  const [me, setMe] = useState<UserRead | null>(null);
  const [meRead, setMeRead] = useState(0);
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

  /** Forgets this tab's session; the login page follows. */
  const signOut = useCallback(() => {
    api.session = undefined;
    setSession(null);
    setMe(null);
  }, []);

  /** Logs out: this session, or (`everywhere`) every one the user issued. */
  const logout = useCallback(
    (everywhere: boolean) => {
      api
        .logout({ all: everywhere })
        .catch((error: unknown) => {
          if (everywhere) fail(error);
        })
        .finally(signOut);
    },
    [fail, signOut],
  );

  useEffect(() => {
    whenUnauthorized(() => {
      signOut();
      notify('The session ended. Log in again.', true);
    });
  }, [signOut, notify]);

  // The very first call: a fresh install shows the setup page instead.
  useEffect(() => {
    versionGet(api)
      .then((version) => {
        const instance = version.settings.instance;
        if (typeof instance === 'string' && instance) setName(instance);
        setRoot(version.missing_root ? 'missing' : 'present');
      })
      .catch((error: unknown) => {
        setRoot('present');
        fail(error);
      });
  }, [fail]);

  // The logged-in user, read with this tab's session.
  useEffect(() => {
    if (token === null || root !== 'present') return;
    let live = true;
    ownUserGet(api)
      .then((read) => {
        if (live) setMe(read);
      })
      .catch((error: unknown) => {
        if (live) fail(error);
      });
    return () => {
      live = false;
    };
  }, [token, root, meRead, fail]);

  const open = root === 'present' && token !== null && me !== null;
  const reads = me !== null && api.session?.can(usersListOperation) === true;

  // `users/me` was the own page; `users` without `users.$read` shows nothing.
  useEffect(() => {
    if (view !== 'users' || me === null) return;
    if (segments()[1] === 'me' || !reads) go(['user'], true);
  }, [view, me, reads]);

  useEffect(() => {
    document.title = name;
  }, [name]);

  // The wallets and the assets they can be made in.
  useEffect(() => {
    if (!open) return;
    let live = true;
    Promise.all([assetsList(api), allWallets(), networksList(api)])
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
  }, [open, token, view, tagged, fail]);

  useEffect(() => {
    const navigate = () => setView(linked());
    window.addEventListener('popstate', navigate);
    return () => window.removeEventListener('popstate', navigate);
  }, []);

  // Each state shows only the views it may: setup, login, or the dashboard.
  useEffect(() => {
    if (root === 'unknown') return;
    const want = allowed(view, root, token);
    if (want !== view) go([want], true);
  }, [root, token, view]);

  /** A new session for this tab: kept, and used from the next call on. */
  const keep = useCallback((value: string) => {
    api.session = Session.parse(value);
    setSession(value);
  }, []);

  /** A session from the login page: the dashboard opens. */
  const signIn = (value: string) => {
    keep(value);
    go(['dashboard']);
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
          ...(reads
            ? [{ view: 'users' as const, label: 'Users', icon: <People fontSize="small" /> }]
            : []),
          { view: 'docs' as const, label: 'Docs', icon: <MenuBook fontSize="small" /> },
        ]
      : []),
  ];
  const known = root !== 'unknown';
  const shown = allowed(view, root, token);

  // Logged out: no bar and no menu, only the login or setup page.
  const chrome = root === 'present' && token !== null;

  if (!known) return null;

  return (
    // The page is at least the screen's height; `main` fills what the bar leaves.
    <Box sx={{ display: 'flex', flexDirection: 'column', minHeight: '100dvh' }}>
      {chrome && (
        <AppBar>
          <Toolbar sx={{ gap: wide ? 4 : 1, flexWrap: 'wrap', minHeight: 60 }}>
            {!wide && views.length > 0 && (
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
              {name}
            </Link>
            {wide && views.length > 0 && (
              <Tabs
                value={views.some((v) => v.view === view) ? view : false}
                onChange={(_, next: View) => go([next])}
                aria-label="Views"
              >
                {views.map((v) => (
                  <Tab key={v.view} value={v.view} label={v.label} />
                ))}
              </Tabs>
            )}
            <Box sx={{ flexGrow: 1 }} />
            {open && (
              <Link
                href="/user"
                underline="none"
                color="text.primary"
                aria-label={me.user.name || me.user.email}
                onClick={(e) => {
                  e.preventDefault();
                  go(['user']);
                }}
                sx={{ display: 'flex', alignItems: 'center', gap: 1 }}
              >
                <Avatar sx={{ width: 28, height: 28, fontSize: 12 }}>{initials(me.user)}</Avatar>
                {wide && <Typography sx={{ fontSize: 14 }}>{me.user.name}</Typography>}
              </Link>
            )}
          </Toolbar>
        </AppBar>
      )}
      {chrome && (
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
              {name}
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
        </Drawer>
      )}

      {open && (
        <Explorers.Provider value={explorers}>
          <Box component="main" sx={{ flexGrow: 1, display: 'flex', flexDirection: 'column' }}>
            {view === 'dashboard' && (
              <Dashboard
                shell={shell}
                banner={api.session?.can(recoveryCreateOperation) && <RecoveryBanner />}
              />
            )}
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
            {view === 'users' && reads && (
              <Users shell={shell} me={me} onMe={() => setMeRead((n) => n + 1)} />
            )}
            {view === 'user' && (
              <Me
                shell={shell}
                me={me}
                onMe={() => setMeRead((n) => n + 1)}
                own={{ onLogout: logout, onSignedOut: signOut, onSession: keep }}
              />
            )}
            {view === 'docs' && <Docs shell={shell} secured />}
          </Box>
          <LogsDrawer shell={shell} source={logSource} />
        </Explorers.Provider>
      )}
      {shown === 'setup' && (
        <Setup
          instance={name}
          onSession={keep}
          onInstance={setName}
          onDone={() => {
            setRoot('present');
            go(['dashboard']);
          }}
        />
      )}
      {shown === 'login' && <Login instance={name} onSession={signIn} />}

      <Toast message={toast} onClose={() => setToast(null)} />
    </Box>
  );
}
