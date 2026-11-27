import AccountBalanceWallet from '@mui/icons-material/AccountBalanceWallet';
import Box from '@mui/material/Box';
import type { Wallet as Account } from '@hashinbox/client';
import { Empty } from '@/_components/empty';
import { Rail } from './_components/rail';
import { Wallet } from './_components/wallet';
import { Split } from '@/_components/split';

interface Props {
  wallets: Account[];
  selected: string | null;
  onSelect: (id: string) => void;
  /** Closes the selected wallet. */
  onClose: () => void;
  onAdded: (wallet: Account) => void;
  /** Tags by wallet id. */
  tags: Map<string, string[]>;
  /** Called after a tag was added or removed. */
  onTagged: () => void;
}

/** The wallet list beside the one wallet it has selected. */
export function Wallets({ wallets, selected, onSelect, onClose, onAdded, tags, onTagged }: Props) {
  const current = wallets.find((w) => w.id === selected) ?? null;
  return (
    <Split
      columns="minmax(260px, 320px) minmax(0, 1fr)"
      open={current !== null}
      onClose={onClose}
      list={
        <Rail
          wallets={wallets}
          selected={selected}
          onSelect={onSelect}
          onAdded={onAdded}
          tags={tags}
        />
      }
      detail={
        <Box sx={{ px: { xs: 2, md: 4 }, py: { xs: 2.5, md: 3.5 }, minWidth: 0 }}>
          {current ? (
            <Wallet
              key={current.id}
              wallet={current}
              tags={tags.get(current.id) ?? []}
              onTagged={onTagged}
              onWatched={onAdded}
            />
          ) : (
            <Empty icon={<AccountBalanceWallet />} title="Wallets">
              A wallet is one address on one network, with its key kept here or watched only. Pick
              one to see its balance and transactions.
            </Empty>
          )}
        </Box>
      }
    />
  );
}
