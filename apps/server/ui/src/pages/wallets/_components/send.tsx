import Accordion from '@mui/material/Accordion';
import AccordionDetails from '@mui/material/AccordionDetails';
import AccordionSummary from '@mui/material/AccordionSummary';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import { call, client, type Wallet } from '@/api/client';
import type { Shell } from '@/layout';
import { assetOf, shorten } from '@/money';

export function Send({
  shell,
  wallet,
  onSent,
}: {
  shell: Shell;
  wallet: Wallet;
  onSent: () => void;
}) {
  const [destination, setDestination] = useState('');
  const [amount, setAmount] = useState('');
  const [note, setNote] = useState('');
  const [busy, setBusy] = useState(false);

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setNote('');
    const params = { path: { id: wallet.id } };
    const body = {
      asset: assetOf(wallet),
      transfers: [{ destination: destination.trim(), amount: amount.trim() }],
    };
    call(client.POST('/v1/wallets/{id}/transactions', { params, body }))
      .then((accepted) => {
        setNote(`queued as job ${shorten(accepted.job)}`);
        setDestination('');
        setAmount('');
        shell.notify('Queued. It appears in the history once it is mined.');
        onSent();
      })
      .catch(shell.fail)
      .finally(() => setBusy(false));
  };

  return (
    <Accordion>
      <AccordionSummary
        expandIcon={
          <Box component="span" sx={{ fontSize: 20, lineHeight: 1 }}>
            +
          </Box>
        }
      >
        <Typography variant="h4" component="span">
          Send
        </Typography>
      </AccordionSummary>
      <AccordionDetails>
        <Box
          component="form"
          autoComplete="off"
          onSubmit={submit}
          sx={{
            display: 'grid',
            gridTemplateColumns: { xs: 'minmax(0, 1fr)', md: 'minmax(0, 2fr) minmax(0, 1fr)' },
            gap: 2.5,
          }}
        >
          <TextField
            label="To"
            value={destination}
            onChange={(e) => setDestination(e.target.value)}
            slotProps={{ htmlInput: { spellCheck: false } }}
            required
          />
          <TextField
            label="Amount"
            placeholder="0.00"
            value={amount}
            onChange={(e) => setAmount(e.target.value)}
            required
            slotProps={{ htmlInput: { inputMode: 'decimal' } }}
          />
          <Stack direction="row" spacing={2} sx={{ alignItems: 'center', gridColumn: '1 / -1' }}>
            <Button type="submit" variant="contained" loading={busy}>
              Send
            </Button>
            <Typography variant="body2" color="text.secondary">
              {note}
            </Typography>
          </Stack>
        </Box>
      </AccordionDetails>
    </Accordion>
  );
}
