import Accordion from '@mui/material/Accordion';
import AccordionDetails from '@mui/material/AccordionDetails';
import AccordionSummary from '@mui/material/AccordionSummary';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Checkbox from '@mui/material/Checkbox';
import FormControlLabel from '@mui/material/FormControlLabel';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import { call, client, type Wallet } from '@/api/client';
import type { Shell } from '@/layout';
import { shorten } from '@/money';

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
  const [everything, setEverything] = useState(false);
  const [note, setNote] = useState('');
  const [busy, setBusy] = useState(false);

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setNote('');
    const params = { path: { id: wallet.id } };
    const body = { destination: destination.trim(), amount: everything ? 'all' : amount.trim() };
    call(client.POST('/v1/wallets/{id}/transactions', { params, body }))
      .then((accepted) => {
        setNote(`submitted as ${shorten(accepted.transaction_id)}`);
        setDestination('');
        setAmount('');
        shell.notify('Submitted. It appears as pending once the node reports it.');
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
            value={everything ? 'all' : amount}
            onChange={(e) => setAmount(e.target.value)}
            disabled={everything}
            required={!everything}
            slotProps={{ htmlInput: { inputMode: 'decimal' } }}
          />
          <FormControlLabel
            sx={{ gridColumn: '1 / -1' }}
            control={
              <Checkbox
                size="small"
                checked={everything}
                onChange={(e) => setEverything(e.target.checked)}
              />
            }
            label="Everything after fees"
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
