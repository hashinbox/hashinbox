import Alert from '@mui/material/Alert';
import Snackbar from '@mui/material/Snackbar';

export interface Message {
  text: string;
  bad: boolean;
}

/** One transient line at the bottom of the screen. */
export function Toast({ message, onClose }: { message: Message | null; onClose: () => void }) {
  return (
    <Snackbar
      open={message !== null}
      autoHideDuration={message?.bad ? 7000 : 3500}
      onClose={onClose}
      anchorOrigin={{ vertical: 'bottom', horizontal: 'left' }}
    >
      <Alert
        onClose={onClose}
        severity={message?.bad ? 'error' : 'success'}
        variant="filled"
        icon={false}
        sx={{ minWidth: 280 }}
      >
        {message?.text}
      </Alert>
    </Snackbar>
  );
}
