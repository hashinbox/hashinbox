import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Dialog from '@mui/material/Dialog';
import DialogActions from '@mui/material/DialogActions';
import DialogContent from '@mui/material/DialogContent';
import DialogContentText from '@mui/material/DialogContentText';
import DialogTitle from '@mui/material/DialogTitle';
import TextField from '@mui/material/TextField';
import { useState, type ReactNode, type SubmitEvent } from 'react';
import { mono } from '@/theme';

interface Props {
  open: boolean;
  title: string;
  children: ReactNode;
  /** The text the operator must type to proceed; it names what is lost. */
  phrase: string;
  action: string;
  onClose: () => void;
  onConfirm: () => void;
}

/** A destructive action that proceeds only after its target is typed out. */
export function Confirm({ open, title, children, phrase, action, onClose, onConfirm }: Props) {
  const [typed, setTyped] = useState('');
  const close = () => {
    setTyped('');
    onClose();
  };
  return (
    <Dialog open={open} onClose={close} fullWidth maxWidth="xs">
      <Box
        component="form"
        onSubmit={(event: SubmitEvent<HTMLFormElement>) => {
          event.preventDefault();
          if (typed !== phrase) return;
          setTyped('');
          onConfirm();
        }}
      >
        <DialogTitle variant="h3">{title}</DialogTitle>
        <DialogContent>
          <DialogContentText component="div" sx={{ fontSize: 13, mb: 2 }}>
            {children}
          </DialogContentText>
          <TextField
            label={
              <span>
                Type{' '}
                <Box component="span" sx={{ fontFamily: mono }}>
                  {phrase}
                </Box>{' '}
                to confirm
              </span>
            }
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            slotProps={{ htmlInput: { spellCheck: false } }}
            autoFocus
          />
        </DialogContent>
        <DialogActions>
          <Button color="inherit" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" variant="contained" color="error" disabled={typed !== phrase}>
            {action}
          </Button>
        </DialogActions>
      </Box>
    </Dialog>
  );
}
