import Delete from '@mui/icons-material/Delete';
import Alert from '@mui/material/Alert';
import Autocomplete from '@mui/material/Autocomplete';
import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import Chip from '@mui/material/Chip';
import Collapse from '@mui/material/Collapse';
import FormControlLabel from '@mui/material/FormControlLabel';
import IconButton from '@mui/material/IconButton';
import MenuItem from '@mui/material/MenuItem';
import Stack from '@mui/material/Stack';
import Switch from '@mui/material/Switch';
import TextField from '@mui/material/TextField';
import ToggleButton from '@mui/material/ToggleButton';
import ToggleButtonGroup from '@mui/material/ToggleButtonGroup';
import Typography from '@mui/material/Typography';
import { useState, type ReactNode } from 'react';
import type { Wallet } from '@/api/client';
import type { Shell } from '@/layout';
import { assetName } from '@/money';
import { mono } from '@/theme';
import { ActionPart, type Choices } from './action';
import {
  AGGREGATES,
  OPERATORS,
  compileCheck,
  compileFilter,
  compileSelector,
  parseCheck,
  parseFilter,
  parseSelector,
  type CompareForm,
  type Definition,
} from './forms';

interface Props {
  shell: Shell;
  wallets: Wallet[];
  /** Every tag in use, offered when choosing wallets. */
  tags: string[];
  value: Definition;
  onChange: (value: Definition) => void;
  /** Whether it is stored: its id then never changes. Its network is always the caller's. */
  kept: boolean;
}

/**
 * Any trigger, part by part: its id and network, target, filter, check and
 * action, with a JSON view for what the forms cannot show. It holds no
 * state of its own beyond that view.
 */
export function TriggerForm({ shell, wallets, tags, value: def, onChange, kept }: Props) {
  const [json, setJson] = useState(false);
  const assets = shell.listings
    .filter((l) => l.network.chain === def.chain && l.network.name === def.network)
    .map((l) => l.asset);
  const name = (asset: string) => assetName(shell.listings, def.chain, def.network, asset);
  const choices: Choices = { chain: def.chain, network: def.network, assets, name, wallets, tags };
  const update = (patch: Partial<Definition>) => {
    onChange({ ...def, ...patch });
  };

  return (
    <Box>
      <Stack direction="row" spacing={2} sx={{ alignItems: 'flex-start', mb: 3, flexWrap: 'wrap' }}>
        {!kept && (
          <TextField
            label="Id"
            placeholder="sweep-deposits"
            value={def.id}
            onChange={(e) => update({ id: e.target.value.toLowerCase() })}
            slotProps={{ htmlInput: { pattern: '[a-z0-9-]{1,64}', spellCheck: false } }}
            helperText="1 to 64 of a-z, 0-9 and -; never changes."
            required
          />
        )}
        <Chip label={`${def.chain}/${def.network}`} sx={{ fontFamily: mono, mt: 1 }} />
        <Box sx={{ flexGrow: 1 }} />
        <FormControlLabel
          control={
            <Switch checked={def.enabled} onChange={(e) => update({ enabled: e.target.checked })} />
          }
          label={def.enabled ? 'Enabled' : 'Disabled'}
        />
      </Stack>

      <Section
        title="Target"
        hint="The wallet indexes this trigger acts on. Tag wallets on the Wallets page."
      >
        <SelectorPart
          def={def}
          assets={assets}
          name={name}
          knownTags={tags}
          onChange={(selector) => update({ selector })}
        />
      </Section>

      <Section
        title="Filter"
        hint="Which of them take part: each wallet is tested on its own; all compares must hold."
      >
        <FilterPart
          def={def}
          assets={assets}
          name={name}
          onChange={(filter) => update({ filter })}
        />
      </Section>

      <Section title="Check" hint="When it acts, over the wallets the filter kept. All must hold.">
        <CheckPart def={def} assets={assets} name={name} onChange={(check) => update({ check })} />
      </Section>

      <Section title="Action" hint="What it sends.">
        <ActionPart
          action={def.action}
          choices={choices}
          onChange={(action) => update({ action })}
        />
      </Section>

      <Stack direction="row" spacing={1.5} sx={{ mt: 1, mb: 2 }}>
        <Button color="inherit" onClick={() => setJson(!json)}>
          {json ? 'Hide JSON' : 'Edit as JSON'}
        </Button>
      </Stack>

      <Collapse in={json} unmountOnExit>
        <JsonPart
          key={JSON.stringify([def.selector, def.filter, def.check, def.action])}
          def={def}
          onChange={(next) =>
            update({ ...next, id: def.id, chain: def.chain, network: def.network })
          }
        />
      </Collapse>
    </Box>
  );
}

function Section({ title, hint, children }: { title: string; hint: string; children: ReactNode }) {
  return (
    <Box sx={{ pb: 2.5, mb: 2.5, borderBottom: 1, borderColor: 'divider' }}>
      <Typography variant="h4" component="h2">
        {title}
      </Typography>
      <Typography variant="body2" color="text.secondary" sx={{ mb: 1.5 }}>
        {hint}
      </Typography>
      {children}
    </Box>
  );
}

/** A part the forms cannot show; edited as JSON. */
function Custom() {
  return (
    <Alert severity="info" variant="outlined">
      Written by hand; edit it as JSON below.
    </Alert>
  );
}

function SelectorPart({
  def,
  assets,
  name,
  knownTags,
  onChange,
}: {
  def: Definition;
  assets: string[];
  name: (asset: string) => string;
  knownTags: string[];
  onChange: (selector: Definition['selector']) => void;
}) {
  const form = parseSelector(def.selector);
  if (!form) return <Custom />;
  const set = (patch: Partial<typeof form>) => onChange(compileSelector({ ...form, ...patch }));
  const tags = (label: string, value: string[], key: 'tags' | 'exclude') => (
    <Autocomplete
      multiple
      freeSolo
      options={knownTags}
      value={value}
      onChange={(_, next) =>
        set({ [key]: next.map((t) => t.trim().toLowerCase()).filter(Boolean) })
      }
      renderInput={(params) => <TextField {...params} label={label} placeholder="tag" />}
      sx={{ minWidth: 220, flexGrow: 1 }}
    />
  );
  return (
    <Box sx={{ display: 'grid', gap: 2, gridTemplateColumns: { md: '160px 1fr auto' } }}>
      <TextField
        select
        label="Asset"
        value={form.asset}
        onChange={(e) => set({ asset: e.target.value })}
      >
        <MenuItem value="">any</MenuItem>
        {assets.map((a) => (
          <MenuItem key={a} value={a}>
            {name(a)}
          </MenuItem>
        ))}
      </TextField>
      {tags('Tagged', form.tags, 'tags')}
      <ToggleButtonGroup
        size="small"
        exclusive
        value={form.match}
        onChange={(_, match: 'all' | 'any' | null) => match && set({ match })}
      >
        <ToggleButton value="all">every tag</ToggleButton>
        <ToggleButton value="any">any tag</ToggleButton>
      </ToggleButtonGroup>
      <Box sx={{ gridColumn: { md: '2 / 4' } }}>
        {tags('Except tagged', form.exclude, 'exclude')}
      </Box>
    </Box>
  );
}

const BLANK: CompareForm = {
  asset: '',
  aggregate: 'sum',
  operator: 'above',
  amount: 'value',
  value: '0',
};

function Compares({
  rows,
  assets,
  name,
  onChange,
}: {
  rows: CompareForm[];
  assets: string[];
  name: (asset: string) => string;
  onChange: (rows: CompareForm[]) => void;
}) {
  const set = (i: number, patch: Partial<CompareForm>) =>
    onChange(rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  return (
    <Stack spacing={1.25}>
      {rows.map((row, i) => (
        <Stack key={i} direction="row" spacing={1} sx={{ alignItems: 'center', flexWrap: 'wrap' }}>
          <TextField
            select
            size="small"
            label="Aggregate"
            value={row.aggregate}
            onChange={(e) => set(i, { aggregate: e.target.value as CompareForm['aggregate'] })}
            sx={{ width: 110 }}
          >
            {AGGREGATES.map((a) => (
              <MenuItem key={a} value={a}>
                {name(a)}
              </MenuItem>
            ))}
          </TextField>
          <TextField
            select
            size="small"
            label="Of balance in"
            value={row.asset}
            onChange={(e) => set(i, { asset: e.target.value })}
            sx={{ width: 150 }}
          >
            <MenuItem value="">its asset</MenuItem>
            {assets.map((a) => (
              <MenuItem key={a} value={a}>
                {name(a)}
              </MenuItem>
            ))}
          </TextField>
          <TextField
            select
            size="small"
            label="Is"
            value={row.operator}
            onChange={(e) => set(i, { operator: e.target.value as CompareForm['operator'] })}
            sx={{ width: 70 }}
          >
            {OPERATORS.map((o) => (
              <MenuItem key={o.value} value={o.value}>
                {o.label}
              </MenuItem>
            ))}
          </TextField>
          <TextField
            select
            size="small"
            label="Than"
            value={row.amount}
            onChange={(e) => set(i, { amount: e.target.value as CompareForm['amount'] })}
            sx={{ width: 140 }}
          >
            <MenuItem value="value">a value</MenuItem>
            <MenuItem value="fee">the send fee</MenuItem>
            <MenuItem value="balance">its balance</MenuItem>
          </TextField>
          {row.amount === 'value' && (
            <TextField
              size="small"
              label={row.aggregate === 'count' ? 'Count' : 'Display units'}
              value={row.value}
              onChange={(e) => set(i, { value: e.target.value })}
              slotProps={{ htmlInput: { inputMode: 'decimal' } }}
              sx={{ width: 140 }}
            />
          )}
          <IconButton
            size="small"
            aria-label="Remove compare"
            onClick={() => onChange(rows.filter((_, j) => j !== i))}
          >
            <Delete fontSize="small" />
          </IconButton>
        </Stack>
      ))}
      <Box>
        <Button size="small" onClick={() => onChange([...rows, BLANK])}>
          Add compare
        </Button>
      </Box>
    </Stack>
  );
}

function FilterPart({
  def,
  assets,
  name,
  onChange,
}: {
  def: Definition;
  assets: string[];
  name: (asset: string) => string;
  onChange: (filter: Definition['filter']) => void;
}) {
  const form = parseFilter(def.filter);
  if (!form) return <Custom />;
  return (
    <Compares
      rows={form}
      assets={assets}
      name={name}
      onChange={(rows) => onChange(compileFilter(rows))}
    />
  );
}

function CheckPart({
  def,
  assets,
  name,
  onChange,
}: {
  def: Definition;
  assets: string[];
  name: (asset: string) => string;
  onChange: (check: Definition['check']) => void;
}) {
  const form = parseCheck(def.check);
  if (!form) return <Custom />;
  const set = (patch: Partial<typeof form>) => onChange(compileCheck({ ...form, ...patch }));
  return (
    <Stack spacing={1.5}>
      <Stack direction="row" spacing={3} sx={{ alignItems: 'center', flexWrap: 'wrap' }}>
        <TextField
          size="small"
          type="number"
          label="Seconds since last run"
          value={form.since}
          onChange={(e) => set({ since: Math.max(0, Number(e.target.value) || 0) })}
          helperText="0: no wait"
          sx={{ width: 200 }}
        />
      </Stack>
      <Compares
        rows={form.compares}
        assets={assets}
        name={name}
        onChange={(compares) => set({ compares })}
      />
    </Stack>
  );
}

/** The whole definition, but its id and network, as editable JSON; remounted on change. */
function JsonPart({ def, onChange }: { def: Definition; onChange: (def: Definition) => void }) {
  const shown = JSON.stringify(
    { selector: def.selector, filter: def.filter, check: def.check, action: def.action },
    null,
    2,
  );
  const [text, setText] = useState(shown);
  const [problem, setProblem] = useState<string | null>(null);
  return (
    <Box sx={{ mb: 3 }}>
      <TextField
        multiline
        fullWidth
        minRows={10}
        value={text}
        onChange={(e) => setText(e.target.value)}
        slotProps={{ htmlInput: { spellCheck: false, sx: { fontFamily: mono, fontSize: 12.5 } } }}
      />
      {problem && (
        <Alert severity="error" sx={{ mt: 1 }}>
          {problem}
        </Alert>
      )}
      <Button
        size="small"
        sx={{ mt: 1 }}
        onClick={() => {
          try {
            const parsed = JSON.parse(text) as Partial<Definition>;
            setProblem(null);
            onChange({ ...def, ...parsed });
          } catch (error) {
            setProblem(error instanceof Error ? error.message : String(error));
          }
        }}
      >
        Apply JSON
      </Button>
    </Box>
  );
}
