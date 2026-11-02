import AccountBalanceWallet from '@mui/icons-material/AccountBalanceWallet';
import Box from '@mui/material/Box';
import type { Wallet as Account } from '@/api/client';
import type { Shell } from '@/layout';
import { Empty } from '@/_components/empty';
import { Rail } from './_components/rail';
import { Wallet } from './_components/wallet';

interface Props {
  shell: Shell;
  wallets: Account[];
  selected: string | null;
  onSelect: (id: string) => void;
  onAdded: (wallet: Account) => void;
  /** Tags by wallet id. */
  tags: Map<string, string[]>;
  /** Called after a tag was added or removed. */
  onTagged: () => void;
}

/** The wallet list beside the one wallet it has selected. */
export function Wallets({ shell, wallets, selected, onSelect, onAdded, tags, onTagged }: Props) {
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
        tags={tags}
      />
      <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
        {current ? (
          <Wallet
            key={current.id}
            shell={shell}
            wallet={current}
            tags={tags.get(current.id) ?? []}
            onTagged={onTagged}
            onWatched={onAdded}
          />
        ) : (
          <Empty icon={<AccountBalanceWallet />} title="Wallets">
            A wallet is one address on one network, with its key kept here or watched only. Pick one
            to see its balance and transactions.
          </Empty>
        )}
      </Box>
    </Box>
  );
}
