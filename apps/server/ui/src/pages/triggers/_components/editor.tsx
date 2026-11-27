import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Stack from '@mui/material/Stack';
import Typography from '@mui/material/Typography';
import { useState } from 'react';
import { triggerReplace, type Trigger, type Wallet } from '@hashinbox/client';
import { api } from '@/api/client';
import { mono } from '@/theme';
import { TriggerForm } from './form';
import type { Definition } from './forms';
import { fail, notify } from '@/store/app';

/** A stored trigger's form: saving it, or going back without. */
export function Editor({
  wallets,
  tags,
  initial,
  onSaved,
  onCancel,
}: {
  wallets: Wallet[];
  tags: string[];
  initial: Definition;
  onSaved: (trigger: Trigger) => void;
  onCancel: () => void;
}) {
  const [def, setDef] = useState<Definition>(initial);
  const [busy, setBusy] = useState(false);

  const save = () => {
    setBusy(true);
    const { id, ...body } = def;
    triggerReplace(api, { id, body })
      .then((saved) => {
        notify('Trigger saved.');
        onSaved(saved);
      })
      .catch(fail)
      .finally(() => setBusy(false));
  };

  return (
    <Box component="article" sx={{ maxWidth: 900 }}>
      <Stack direction="row" spacing={2} sx={{ alignItems: 'center', mb: 2 }}>
        <Typography variant="h2" component="h1" sx={{ fontFamily: mono, flexGrow: 1 }}>
          {def.id}
        </Typography>
        <Button variant="text" onClick={onCancel}>
          Cancel
        </Button>
        <Button variant="contained" loading={busy} onClick={save}>
          Save
        </Button>
      </Stack>
      <TriggerForm wallets={wallets} tags={tags} value={def} onChange={setDef} kept />
    </Box>
  );
}
