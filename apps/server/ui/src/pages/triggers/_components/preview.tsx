import Alert from '@mui/material/Alert';
import Box from '@mui/material/Box';
import Chip from '@mui/material/Chip';
import Stack from '@mui/material/Stack';
import Table from '@mui/material/Table';
import TableBody from '@mui/material/TableBody';
import TableCell from '@mui/material/TableCell';
import TableHead from '@mui/material/TableHead';
import TableRow from '@mui/material/TableRow';
import Typography from '@mui/material/Typography';
import type { TriggerPreview } from '@/api/client';
import { EmptyRow } from '@/_components/table';
import { shorten } from '@/money';
import { mono } from '@/theme';

/** What a trigger sees and would do now: each wallet kept or not, the check, the transfers. */
export function Preview({ preview }: { preview: TriggerPreview }) {
  const kept = preview.selected.filter((s) => s.kept).length;
  return (
    <Box sx={{ mb: 3 }}>
      <Stack direction="row" spacing={1} sx={{ alignItems: 'center', mb: 1, flexWrap: 'wrap' }}>
        <Typography variant="h4" component="h2" sx={{ mr: 1 }}>
          Now
        </Typography>
        <Typography sx={{ fontFamily: mono, fontSize: 12, color: 'text.secondary' }}>
          {preview.confirmed == null ? 'no block kept yet' : `at block ${preview.confirmed}`}
        </Typography>
        <Chip size="small" label={`filter keeps ${kept} of ${preview.selected.length}`} />
        <Chip
          size="small"
          label={preview.check ? 'check holds' : 'check does not hold'}
          color={preview.check ? 'success' : 'warning'}
        />
      </Stack>
      <Table size="small">
        <TableHead>
          <TableRow>
            <TableCell>Address</TableCell>
            <TableCell>Asset</TableCell>
            <TableCell align="right">Confirmed</TableCell>
            <TableCell align="right">Pending</TableCell>
            <TableCell>Filter</TableCell>
          </TableRow>
        </TableHead>
        <TableBody>
          {preview.selected.length === 0 && (
            <EmptyRow columns={5} message="The target picks no fully indexed wallet." />
          )}
          {preview.selected.map((s) => (
            <TableRow key={`${s.wallet_id}:${s.asset}`} sx={{ opacity: s.kept ? 1 : 0.5 }}>
              <TableCell sx={{ fontFamily: mono, fontSize: 12 }}>{s.address}</TableCell>
              <TableCell>{s.asset}</TableCell>
              <TableCell align="right" sx={{ fontFamily: mono }}>
                {s.confirmed}
              </TableCell>
              <TableCell align="right" sx={{ fontFamily: mono, color: 'text.secondary' }}>
                {s.pending}
              </TableCell>
              <TableCell>{s.kept ? 'kept' : 'left out'}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>

      <Typography variant="h4" component="h3" sx={{ mt: 2.5, mb: 1 }}>
        Would send
      </Typography>
      {preview.plan_error && (
        <Alert severity="error" sx={{ mb: 1 }}>
          {preview.plan_error}
        </Alert>
      )}
      {preview.unkeyed.length > 0 && (
        <Alert severity="warning" sx={{ mb: 1 }}>
          No key here for {preview.unkeyed.map(shorten).join(', ')}: a run refuses them.
        </Alert>
      )}
      <Table size="small">
        <TableHead>
          <TableRow>
            <TableCell>From</TableCell>
            <TableCell>To</TableCell>
            <TableCell align="right">Amount</TableCell>
          </TableRow>
        </TableHead>
        <TableBody>
          {preview.transfers.length === 0 && (
            <EmptyRow columns={3} message="Nothing to send now." />
          )}
          {preview.transfers.map((t, i) => (
            <TableRow key={i}>
              <TableCell sx={{ fontFamily: mono, fontSize: 12 }} title={t.from}>
                {shorten(t.from)}
              </TableCell>
              <TableCell sx={{ fontFamily: mono, fontSize: 12 }} title={t.to}>
                {shorten(t.to)}
              </TableCell>
              <TableCell align="right" sx={{ fontFamily: mono }}>
                {t.amount}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      {!preview.check && preview.transfers.length > 0 && (
        <Typography variant="body2" color="text.secondary" sx={{ mt: 1 }}>
          The check does not hold now, so a run would not send these yet.
        </Typography>
      )}
    </Box>
  );
}
