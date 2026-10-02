import Link from '@mui/material/Link';

/** An inline "copy" action; reports through `onDone`. */
export function Copy({
  text,
  label,
  onDone,
}: {
  text: string;
  label: string;
  onDone: (message: string) => void;
}) {
  return (
    <Link
      component="button"
      type="button"
      color="text.secondary"
      underline="always"
      sx={{ fontSize: 12, verticalAlign: 'baseline' }}
      onClick={() => {
        navigator.clipboard.writeText(text).then(
          () => onDone('Copied.'),
          () => onDone(`Copy by hand: ${text}`),
        );
      }}
    >
      {label}
    </Link>
  );
}
