import TableCell from '@mui/material/TableCell';
import TableRow from '@mui/material/TableRow';

/** One compact empty state shared by data tables. */
export function EmptyRow({ columns, message }: { columns: number; message: string }) {
  return (
    <TableRow>
      <TableCell colSpan={columns} sx={{ color: 'text.secondary', height: 40 }}>
        {message}
      </TableCell>
    </TableRow>
  );
}
