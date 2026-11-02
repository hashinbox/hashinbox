import Check from '@mui/icons-material/Check';
import ContentCopy from '@mui/icons-material/ContentCopy';
import Button from '@mui/material/Button';
import Divider from '@mui/material/Divider';
import Stack from '@mui/material/Stack';
import Tooltip from '@mui/material/Tooltip';
import { useState, type ReactElement } from 'react';
import type { Action } from './actions';

/** A light hover tooltip below-right of `children`: one rounded button per action, `Copy | Explorer`. */
export function Hover({ actions, children }: { actions: Action[]; children: ReactElement }) {
  if (actions.length === 0) return children;
  return (
    <Tooltip
      enterDelay={200}
      leaveDelay={150}
      placement="bottom-end"
      slotProps={{
        tooltip: {
          sx: {
            p: 0.25,
            borderRadius: '999px',
            bgcolor: 'grey.100',
            color: 'text.secondary',
            border: 1,
            borderColor: 'grey.200',
            boxShadow: '0 2px 8px rgba(0, 0, 0, 0.08)',
          },
        },
      }}
      title={
        <Stack
          direction="row"
          spacing={0.25}
          divider={
            <Divider orientation="vertical" flexItem sx={{ my: 0.75, borderColor: 'grey.300' }} />
          }
        >
          {actions.map((action) => (
            <ActionButton key={action.label} action={action} />
          ))}
        </Stack>
      }
    >
      {children}
    </Tooltip>
  );
}

/** One action as a small rounded button, icon and label; a copy turns into a check. */
function ActionButton({ action }: { action: Action }) {
  const [copied, setCopied] = useState(false);
  const { copy, href } = action;
  const run = () => {
    if (copy !== undefined) {
      void navigator.clipboard.writeText(copy).then(() => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1200);
      });
    }
    action.onClick?.();
  };
  const icon = copy === undefined ? action.icon : copied ? <Check /> : <ContentCopy />;
  const link = href ? { href, target: '_blank', rel: 'noopener noreferrer' } : {};
  return (
    <Button
      size="small"
      variant="text"
      color="inherit"
      startIcon={icon}
      onClick={run}
      {...link}
      sx={{
        py: 0.25,
        px: 1.25,
        minWidth: 0,
        borderRadius: '999px',
        fontSize: 11.5,
        lineHeight: 1.6,
        textTransform: 'none',
        color: copied ? 'success.main' : 'text.secondary',
        '&:hover': { bgcolor: 'grey.200', color: 'text.primary' },
        '& .MuiButton-startIcon': { mr: 0.5 },
        '& .MuiButton-startIcon svg': { fontSize: 14 },
      }}
    >
      {copied ? 'Copied' : action.label}
    </Button>
  );
}
