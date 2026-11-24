import Box from '@mui/material/Box';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import { useState } from 'react';
import type { Row } from './entries';
import { cell, COLUMNS, duration, failedCall, figures, TONE } from './render';

/** A job run's header: `attempt N · ack · ms · reason`, from its `job.run`. */
export function RunHead({ attempt, end }: { attempt: string; end: Row | undefined }) {
  const f = end?.entry.fields ?? {};
  const text = [
    attempt === '' ? 'outside a run' : `attempt ${attempt}`,
    f.ack,
    f.ms !== undefined && duration(f.ms),
    f.reason,
  ]
    .filter(Boolean)
    .join(' · ');
  return (
    <TableRow>
      <TableCell
        colSpan={COLUMNS}
        sx={{
          ...cell,
          fontWeight: 600,
          bgcolor: 'action.hover',
          color: end ? (TONE[end.entry.level] ?? 'text.primary') : 'text.primary',
        }}
      >
        {text}
      </TableCell>
    </TableRow>
  );
}

/** A run's node calls on one line, `N requests, M failed`; clicked, a table of them. */
export function Calls({ calls }: { calls: Row[] }) {
  const [open, setOpen] = useState(false);
  const failed = calls.filter((row) => failedCall(row.entry)).length;
  return (
    <>
      <TableRow hover onClick={() => setOpen(!open)} sx={{ cursor: 'pointer' }}>
        <TableCell colSpan={COLUMNS} sx={{ ...cell, color: 'text.secondary', pl: 4 }}>
          {`${calls.length} requests, `}
          <Box component="span" sx={{ color: failed > 0 ? 'error.main' : undefined }}>
            {`${failed} failed`}
          </Box>
        </TableCell>
      </TableRow>
      {open && (
        <TableRow>
          <TableCell colSpan={COLUMNS} sx={{ pl: 4 }}>
            <CallTable calls={calls} />
          </TableCell>
        </TableRow>
      )}
    </>
  );
}

/** Each call: method, node, ms, wait, status; a failed one in the error colour. */
function CallTable({ calls }: { calls: Row[] }) {
  return (
    <Table size="small">
      <TableHead>
        <TableRow>
          {['Method', 'Node', 'ms', 'Wait ms', 'Status'].map((head) => (
            <TableCell key={head}>{head}</TableCell>
          ))}
        </TableRow>
      </TableHead>
      <TableBody>
        {calls.map(({ key, entry }) => {
          const f = entry.fields;
          const slow = entry.target === 'rpc.slow';
          const color = failedCall(entry) || slow ? 'error.main' : undefined;
          const values = [
            f.method,
            f.node,
            f.ms,
            slow ? f.waited_ms : f.wait_ms,
            slow
              ? 'pending'
              : [f.status, f.rpc_errors, f.error].filter((v) => v && v !== '-').join(' '),
          ];
          return (
            <TableRow key={key}>
              {values.map((value, i) => (
                <TableCell key={i} sx={{ ...figures, color }}>
                  {value ?? '-'}
                </TableCell>
              ))}
            </TableRow>
          );
        })}
      </TableBody>
    </Table>
  );
}
