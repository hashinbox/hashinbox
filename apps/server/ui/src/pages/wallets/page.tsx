import Box from '@mui/material/Box';
import Typography from '@mui/material/Typography';
import type { Wallet as Account } from '@/api/client';
import type { Shell } from '@/layout';
import { Rail } from './_components/rail';
import { Wallet } from './_components/wallet';

interface Props {
  shell: Shell;
  wallets: Account[];
  selected: string | null;
  onSelect: (id: string) => void;
  onAdded: (wallet: Account) => void;
}

/** The wallet list beside the one wallet it has selected. */
export function Wallets({ shell, wallets, selected, onSelect, onAdded }: Props) {
  const current = wallets.find((w) => w.id === selected) ?? null;
  return (
    <Box
      sx={{
        display: 'grid',
        gridTemplateColumns: { xs: 'minmax(0, 1fr)', md: 'minmax(260px, 320px) minmax(0, 1fr)' },
        minHeight: 'calc(100vh - 65px)',
      }}
    >
      <Rail
        shell={shell}
        wallets={wallets}
        selected={selected}
        onSelect={onSelect}
        onAdded={onAdded}
      />
      <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
        {current ? (
          <Wallet key={current.id} shell={shell} wallet={current} />
        ) : (
          <Typography variant="h3" color="text.secondary">
            Pick a wallet, or create one.
          </Typography>
        )}
      </Box>
    </Box>
  );
}
