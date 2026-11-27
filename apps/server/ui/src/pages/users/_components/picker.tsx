import Box from '@mui/material/Box';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import ToggleButton from '@mui/material/ToggleButton';
import ToggleButtonGroup from '@mui/material/ToggleButtonGroup';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { mono } from '@/theme';
import { ROOT, allows, full, implied, pack, readable, short, type Resources } from './scopes';

/** A node's level: none, read only, or full. */
type Level = '' | 'read' | 'full';

const RANK: Record<Level, number> = { '': 0, read: 1, full: 2 };

/** The scope of tree node `path` (`''`: the root) at `level`. */
function scopeOf(path: string, level: Level): string {
  const base = path === '' ? ROOT : `${ROOT}.${path}`;
  return level === 'read' ? `${base}:read` : base;
}

/** The strongest level an ancestor of `path` (the root included) holds in `levels`. */
function inherited(path: string, levels: Record<string, Level>): Level {
  const steps = path === '' ? [] : path.split('.');
  const above = ['', ...steps.slice(0, -1).map((_, at) => steps.slice(0, at + 1).join('.'))];
  return above.reduce<Level>(
    (best, at) => (RANK[levels[at] ?? ''] > RANK[best] ? (levels[at] ?? '') : best),
    '',
  );
}

/**
 * The scope tree with a Read / Full choice per node, only `held` ones enabled,
 * plus scopes by id as text; starts at `initial` and reports every change to
 * `onChange`. A node covers its subtree, so a child below its parent's level
 * shows the parent's.
 */
export function Picker({
  resources,
  held,
  initial,
  onChange,
}: {
  resources: Resources;
  held: string[];
  initial: string[];
  onChange: (scopes: string[]) => void;
}) {
  const nodes = ['', ...resources];
  const [levels, setLevels] = useState<Record<string, Level>>(() =>
    Object.fromEntries(
      nodes.flatMap((path): [string, Level][] => {
        if (initial.includes(scopeOf(path, 'full'))) return [[path, 'full']];
        if (initial.includes(scopeOf(path, 'read'))) return [[path, 'read']];
        return [];
      }),
    ),
  );
  const tree = new Set(nodes.flatMap((path) => [scopeOf(path, 'full'), scopeOf(path, 'read')]));
  const [extra, setExtra] = useState(() =>
    initial
      .filter((scope) => !tree.has(scope))
      .map(short)
      .join(' '),
  );

  const scopesOf = (next: Record<string, Level>, nextExtra: string) => [
    ...nodes.flatMap((path) => {
      const own = next[path] ?? '';
      return RANK[own] > RANK[inherited(path, next)] ? [scopeOf(path, own)] : [];
    }),
    ...nextExtra.split(/\s+/).filter(Boolean).map(full),
  ];
  const change = (next: Record<string, Level>, nextExtra: string) => {
    setLevels(next);
    setExtra(nextExtra);
    onChange(scopesOf(next, nextExtra));
  };

  return (
    <Box>
      <Box sx={{ border: 1, borderColor: 'divider', borderRadius: 1, px: 1.5, py: 0.5 }}>
        {nodes.map((path) => {
          const above = inherited(path, levels);
          const own = levels[path] ?? '';
          const shown = RANK[above] > RANK[own] ? above : own;
          const depth = path === '' ? 0 : path.split('.').length;
          const also = path === '' ? [] : implied(path);
          return (
            <Stack key={path} direction="row" sx={{ alignItems: 'center', py: 0.25 }}>
              <Typography
                sx={{ fontFamily: mono, fontSize: 12.5, width: 180, flex: 'none', pl: depth * 1.5 }}
              >
                {path === '' ? 'everything' : (path.split('.').pop() ?? path)}
              </Typography>
              <ToggleButtonGroup
                size="small"
                exclusive
                value={shown}
                onChange={(_, level: Level | null) =>
                  change({ ...levels, [path]: level ?? '' }, extra)
                }
              >
                {(['read', 'full'] as const).map((level) => (
                  <ToggleButton
                    key={level}
                    value={level}
                    disabled={RANK[level] <= RANK[above] || !allows(held, scopeOf(path, level))}
                    sx={{ py: 0, px: 1, fontSize: 12, textTransform: 'none' }}
                  >
                    {level === 'read' ? 'Read' : 'Full'}
                  </ToggleButton>
                ))}
              </ToggleButtonGroup>
              {also.length > 0 && (
                <Typography color="text.secondary" sx={{ fontSize: 11.5, ml: 1.5 }}>
                  also {also.join(', ')}
                </Typography>
              )}
            </Stack>
          );
        })}
      </Box>
      <TextField
        label="By id"
        value={extra}
        onChange={(e) => change(levels, e.target.value)}
        helperText="Space-separated, e.g. wallets[ethereum.mainnet.0xabc]:read"
        slotProps={{ htmlInput: { spellCheck: false, style: { fontFamily: mono } } }}
        sx={{ mt: 2 }}
      />
      <Typography
        sx={{ mt: 1.5, fontFamily: mono, fontSize: 11.5, color: 'text.secondary' }}
        aria-label="Packed scopes"
      >
        {JSON.stringify(pack(scopesOf(levels, extra)))}
      </Typography>
    </Box>
  );
}

/** Scopes one per line: as written, and what each allows. */
export function ScopeList({ scopes }: { scopes: string[] }) {
  if (scopes.length === 0)
    return (
      <Typography color="text.secondary" sx={{ fontSize: 13 }}>
        None.
      </Typography>
    );
  return (
    <Box component="ul" sx={{ m: 0, p: 0, listStyle: 'none' }}>
      {scopes.map((scope) => (
        <Stack component="li" key={scope} direction="row" spacing={1.5} sx={{ py: 0.25 }}>
          <Typography sx={{ fontFamily: mono, fontSize: 12.5, wordBreak: 'break-all' }}>
            {short(scope)}
          </Typography>
          <Typography color="text.secondary" sx={{ fontSize: 12.5 }}>
            {readable(scope)}
          </Typography>
        </Stack>
      ))}
    </Box>
  );
}
