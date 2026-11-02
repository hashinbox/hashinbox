import Alert from '@mui/material/Alert';
import Autocomplete from '@mui/material/Autocomplete';
import Box from '@mui/material/Box';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import type { Wallet } from '@/api/client';
import {
  compileAmount,
  compileTarget,
  indexId,
  parseAmount,
  parseTarget,
  type Action,
  type AmountForm,
  type Target,
  type TargetForm,
} from './forms';

/** What the pickers offer: the network's assets, wallet indexes and tags. */
export interface Choices {
  chain: string;
  network: string;
  assets: string[];
  wallets: Wallet[];
  tags: string[];
}

/** Every wallet index id of the network's wallets, with a label. */
function indexes(choices: Choices): { id: string; label: string }[] {
  return choices.wallets
    .filter((w) => w.network.chain === choices.chain && w.network.name === choices.network)
    .flatMap((w) =>
      w.assets.map((asset) => ({
        id: indexId(choices.chain, choices.network, w.address, asset),
        label: `${w.address.slice(0, 10)}… · ${asset}${w.keyed ? '' : ' · watch only'}`,
      })),
    );
}

/** A target: the indexes in context, chosen wallets, or a tag. */
export function TargetPicker({
  label,
  target,
  choices,
  onChange,
}: {
  label: string;
  target: Target;
  choices: Choices;
  onChange: (target: Target) => void;
}) {
  const form = parseTarget(target);
  if (!form) {
    return <Alert severity="info">{label}: written by hand; edit it as JSON below.</Alert>;
  }
  const set = (next: TargetForm) => onChange(compileTarget(next));
  const options = indexes(choices);
  return (
    <Stack spacing={1.25}>
      <TextField
        select
        label={label}
        value={form.kind}
        onChange={(e) => {
          const kind = e.target.value as TargetForm['kind'];
          set(
            kind === 'this'
              ? { kind, asset: '' }
              : kind === 'ids'
                ? { kind, ids: [] }
                : { kind, tag: choices.tags[0] ?? '', asset: '' },
          );
        }}
      >
        <MenuItem value="this">the trigger&apos;s wallets (This)</MenuItem>
        <MenuItem value="ids">chosen wallets</MenuItem>
        <MenuItem value="tag">wallets with a tag</MenuItem>
      </TextField>
      {form.kind === 'this' && (
        <TextField
          select
          size="small"
          label="Read in"
          value={form.asset}
          onChange={(e) => set({ kind: 'this', asset: e.target.value })}
        >
          <MenuItem value="">their own asset</MenuItem>
          {choices.assets.map((a) => (
            <MenuItem key={a} value={a}>
              {a}
            </MenuItem>
          ))}
        </TextField>
      )}
      {form.kind === 'ids' && (
        <Autocomplete
          multiple
          options={options.map((o) => o.id)}
          getOptionLabel={(id) => options.find((o) => o.id === id)?.label ?? id}
          value={form.ids}
          onChange={(_, ids) => set({ kind: 'ids', ids })}
          renderInput={(params) => <TextField {...params} size="small" label="Wallets" />}
        />
      )}
      {form.kind === 'tag' && (
        <Stack direction="row" spacing={1}>
          <Autocomplete
            freeSolo
            options={choices.tags}
            inputValue={form.tag}
            onInputChange={(_, tag) => set({ ...form, tag: tag.trim().toLowerCase() })}
            renderInput={(params) => <TextField {...params} size="small" label="Tag" />}
            sx={{ flexGrow: 1 }}
          />
          <TextField
            select
            size="small"
            label="Asset"
            value={form.asset}
            onChange={(e) => set({ ...form, asset: e.target.value })}
            sx={{ width: 140 }}
          >
            <MenuItem value="">any</MenuItem>
            {choices.assets.map((a) => (
              <MenuItem key={a} value={a}>
                {a}
              </MenuItem>
            ))}
          </TextField>
        </Stack>
      )}
    </Stack>
  );
}

/** How much each transfer moves, from the usual choices. */
function AmountPicker({
  action,
  choices,
  onChange,
}: {
  action: Action;
  choices: Choices;
  onChange: (action: Action) => void;
}) {
  const form = parseAmount(action.input.amount);
  if (!form) return <Alert severity="info">Amount: written by hand; edit it as JSON below.</Alert>;
  const set = (next: AmountForm) =>
    onChange({ name: 'send', input: { ...action.input, amount: compileAmount(next) } });
  const tokens = choices.assets.filter((a) => a !== 'native');
  return (
    <Stack direction="row" spacing={1}>
      <TextField
        select
        label="Amount, per transfer"
        value={form.kind}
        onChange={(e) => {
          const kind = e.target.value as AmountForm['kind'];
          set(
            kind === 'shortfall'
              ? { kind, asset: tokens[0] ?? 'native' }
              : kind === 'value'
                ? { kind, value: '0' }
                : { kind },
          );
        }}
        sx={{ flexGrow: 1 }}
      >
        <MenuItem value="all">everything it holds</MenuItem>
        <MenuItem value="all-less-fee">everything less the fee</MenuItem>
        <MenuItem value="shortfall">the gas a token send lacks</MenuItem>
        <MenuItem value="value">a fixed amount</MenuItem>
      </TextField>
      {form.kind === 'shortfall' && (
        <TextField
          select
          label="Token"
          value={form.asset}
          onChange={(e) => set({ kind: 'shortfall', asset: e.target.value })}
          sx={{ width: 160 }}
        >
          {choices.assets.map((a) => (
            <MenuItem key={a} value={a}>
              {a}
            </MenuItem>
          ))}
        </TextField>
      )}
      {form.kind === 'value' && (
        <TextField
          label="Display units"
          value={form.value}
          onChange={(e) => set({ kind: 'value', value: e.target.value })}
          slotProps={{ htmlInput: { inputMode: 'decimal' } }}
          sx={{ width: 160 }}
        />
      )}
    </Stack>
  );
}

/** A send: what asset moves, from where, to where, and how much. */
export function ActionPart({
  action,
  choices,
  onChange,
}: {
  action: Action;
  choices: Choices;
  onChange: (action: Action) => void;
}) {
  const send = action.input;
  const set = (patch: Partial<Action['input']>) =>
    onChange({ name: 'send', input: { ...send, ...patch } });
  return (
    <Stack spacing={2}>
      <TextField
        select
        label="Send asset"
        value={send.asset}
        onChange={(e) => set({ asset: e.target.value })}
        sx={{ maxWidth: 240 }}
      >
        {choices.assets.map((a) => (
          <MenuItem key={a} value={a}>
            {a}
          </MenuItem>
        ))}
      </TextField>
      <Box sx={{ display: 'grid', gap: 2, gridTemplateColumns: { md: '1fr 1fr' } }}>
        <TargetPicker
          label="From"
          target={send.source}
          choices={choices}
          onChange={(source) => set({ source })}
        />
        <TargetPicker
          label="To"
          target={send.destination}
          choices={choices}
          onChange={(destination) => set({ destination })}
        />
      </Box>
      <AmountPicker action={action} choices={choices} onChange={onChange} />
      <Typography variant="body2" color="text.secondary">
        One side may be many: many senders each pay one recipient, or one sender pays each
        recipient. The amount is worked out for each sender, or each recipient, on its own.
      </Typography>
    </Stack>
  );
}
