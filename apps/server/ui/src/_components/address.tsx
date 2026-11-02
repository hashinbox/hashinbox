import OpenInNew from '@mui/icons-material/OpenInNew';
import Box from '@mui/material/Box';
import Link from '@mui/material/Link';
import { mono } from '@/theme';
import { resolve, type Action, type Actions } from './actions';
import { useExplorer } from './explorers';
import { Hover } from './hover';
import { shorten } from './trim';

/**
 * An address of `network`: trimmed unless `trim` is false, opening the
 * network's explorer in a new tab unless `link` is false or none is configured.
 * Hovered, it offers Copy and Explorer; `actions` replaces or extends those.
 */
export function Address({
  value,
  network,
  trim = true,
  link = true,
  actions,
}: {
  value: string;
  network: { chain: string; name: string };
  trim?: boolean;
  link?: boolean;
  actions?: Actions;
}) {
  const explorer = useExplorer(network);
  const href = explorer?.address.replace('{address}', encodeURIComponent(value));
  const defaults: Action[] = [{ label: 'Copy', copy: value }];
  if (href) defaults.push({ label: 'Explorer', icon: <OpenInNew />, href });
  const text = trim ? shorten(value) : value;
  const sx = { fontFamily: mono, fontSize: 'inherit', overflowWrap: 'anywhere' } as const;
  return (
    <Hover actions={resolve(defaults, actions)}>
      {link && href ? (
        <Link href={href} target="_blank" rel="noopener noreferrer" underline="hover" sx={sx}>
          {text}
        </Link>
      ) : (
        <Box component="span" sx={sx}>
          {text}
        </Box>
      )}
    </Hover>
  );
}
