import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Checkbox from '@mui/material/Checkbox';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogContentText from '@mui/material/DialogContentText';
import DialogTitle from '@mui/material/DialogTitle';
import FormControlLabel from '@mui/material/FormControlLabel';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type SubmitEvent } from 'react';
import { mono } from '@/theme';

/** How many words the check asks back. */
const ASKED = 3;

/** `count` distinct word numbers (0-based) out of `total`, in order. */
function pick(total: number, count = ASKED): number[] {
  const picked: number[] = [];
  while (picked.length < Math.min(count, total)) {
    const n = Math.floor(Math.random() * total);
    if (!picked.includes(n)) picked.push(n);
  }
  return picked.sort((a, b) => a - b);
}

/** Whether each `typed` word is the `asked` one, any case and spacing around. */
function matches(words: string[], asked: number[], typed: string[]): boolean {
  return asked.every((n, i) => (typed[i] ?? '').trim().toLowerCase() === words[n]?.toLowerCase());
}

/** The 12 words, numbered, with the warning and a copy button. */
function Words({ words }: { words: string[] }) {
  const [copied, setCopied] = useState<string | null>(null);
  return (
    <Stack spacing={2}>
      <Alert severity="warning">
        Write these 12 words down and keep them offline. With them, anyone can sign in to any
        account in this organization, except accounts with two-factor on. They&apos;re shown once
        and can&apos;t be shown again.
      </Alert>
      <Box
        component="ol"
        sx={{
          m: 0,
          p: 1.5,
          listStyle: 'none',
          display: 'grid',
          gridTemplateColumns: 'repeat(3, 1fr)',
          gap: 1,
          border: 1,
          borderColor: 'divider',
          borderRadius: 1,
          fontFamily: mono,
          fontSize: 13,
        }}
      >
        {words.map((word, i) => (
          <li key={i}>
            <Typography component="span" color="text.secondary" sx={{ font: 'inherit' }}>
              {i + 1}.
            </Typography>{' '}
            {word}
          </li>
        ))}
      </Box>
      <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
        <Button
          onClick={() => {
            navigator.clipboard.writeText(words.join(' ')).then(
              () => setCopied('Copied.'),
              () => setCopied('Copy failed: write them down by hand.'),
            );
          }}
        >
          Copy
        </Button>
        {copied && (
          <Typography color="text.secondary" sx={{ fontSize: 12.5 }}>
            {copied}
          </Typography>
        )}
      </Stack>
    </Stack>
  );
}

/** One field per `asked` word number, typed back into `typed`. */
function WordCheck({
  asked,
  typed,
  onType,
}: {
  asked: number[];
  typed: string[];
  onType: (typed: string[]) => void;
}) {
  return (
    <Stack spacing={2} sx={{ pt: 1 }}>
      {asked.map((n, i) => (
        <TextField
          key={n}
          label={`Word #${n + 1}`}
          value={typed[i]}
          onChange={(e) => onType(typed.map((t, j) => (j === i ? e.target.value : t)))}
          autoComplete="off"
          slotProps={{ htmlInput: { spellCheck: false, style: { fontFamily: mono } } }}
          autoFocus={i === 0}
          required
        />
      ))}
    </Stack>
  );
}

/**
 * The new words once, with an "I've written them down" checkbox, then three
 * of them typed back before Done. Closing earlier asks first.
 */
export function NewPassphrase({ words, onClose }: { words: string[]; onClose: () => void }) {
  const [written, setWritten] = useState(false);
  const [checking, setChecking] = useState(false);
  const [leaving, setLeaving] = useState(false);
  const [asked] = useState(() => pick(words.length));
  const [typed, setTyped] = useState<string[]>(() => asked.map(() => ''));
  const ok = matches(words, asked, typed);

  const submit = (event: SubmitEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!checking) setChecking(true);
    else if (ok) onClose();
  };

  return (
    <>
      <Dialog open onClose={() => setLeaving(true)} fullWidth maxWidth="sm">
        <Box component="form" onSubmit={submit}>
          <DialogTitle variant="h3">
            {checking ? 'Confirm the passphrase' : 'Recovery passphrase'}
          </DialogTitle>
          <DialogContent>
            {checking ? (
              <>
                <DialogContentText sx={{ fontSize: 13 }}>
                  Type the words with these numbers.
                </DialogContentText>
                <WordCheck asked={asked} typed={typed} onType={setTyped} />
              </>
            ) : (
              <>
                <Words words={words} />
                <FormControlLabel
                  sx={{ mt: 1 }}
                  label="I've written them down"
                  control={
                    <Checkbox checked={written} onChange={(e) => setWritten(e.target.checked)} />
                  }
                />
              </>
            )}
          </DialogContent>
          <DialogActions>
            <Button
              color="inherit"
              onClick={checking ? () => setChecking(false) : () => setLeaving(true)}
            >
              {checking ? 'Back' : 'Close'}
            </Button>
            <Button type="submit" variant="contained" disabled={checking ? !ok : !written}>
              {checking ? 'Done' : 'Continue'}
            </Button>
          </DialogActions>
        </Box>
      </Dialog>
      <Dialog open={leaving} onClose={() => setLeaving(false)} maxWidth="xs">
        <DialogContent>
          <DialogContentText sx={{ fontSize: 13 }}>
            You haven&apos;t confirmed the words; they won&apos;t be shown again. Close anyway?
          </DialogContentText>
        </DialogContent>
        <DialogActions>
          <Button color="inherit" onClick={() => setLeaving(false)}>
            Keep open
          </Button>
          <Button color="error" variant="contained" onClick={onClose}>
            Close anyway
          </Button>
        </DialogActions>
      </Dialog>
    </>
  );
}
