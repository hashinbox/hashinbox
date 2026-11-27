import Box from '@mui/material/Box';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import { useEffect, useState } from 'react';
import { Tags } from '@/_components/tags';
import type { LogSource } from './entries';
import { levelTags, tagOf, type Filter } from './filter';
import { fail, useApp } from '@/store/app';

/**
 * The log view's filter: one tag per level, event, network, wallet, job,
 * transaction or trigger; and a time jump, a local date and time to read back from.
 */
export function Filters({
  source,
  tags,
  onTags,
  jump,
  onJump,
}: {
  source: LogSource;
  tags: Filter[];
  onTags: (tags: Filter[]) => void;
  jump: string;
  onJump: (jump: string) => void;
}) {
  const listings = useApp((s) => s.listings);
  // The events kept so far, as the server lists them.
  const [targets, setTargets] = useState<string[]>([]);
  useEffect(() => {
    source.targets().then(setTargets).catch(fail);
  }, [source]);
  const networks = [...new Set(listings.map((l) => `${l.network.chain}/${l.network.name}`))].sort();
  const options = [
    ...levelTags(),
    ...targets.map((t) => tagOf('target', t)),
    ...networks.map((n) => tagOf('network', n)),
  ];
  return (
    <Stack direction="row" spacing={1.5} sx={{ alignItems: 'flex-start' }}>
      <Box sx={{ flexGrow: 1, minWidth: 0 }}>
        <Tags
          options={options}
          value={tags}
          onChange={onTags}
          placeholder="Filter by level, event, network, wallet, job, transaction or trigger"
          label="Filter logs"
        />
      </Box>
      <TextField
        type="datetime-local"
        size="small"
        value={jump}
        onChange={(event) => onJump(event.target.value)}
        slotProps={{ htmlInput: { 'aria-label': 'Jump to a time' } }}
        sx={{ width: 230, flexShrink: 0, '& input': { fontSize: 13 } }}
      />
    </Stack>
  );
}
