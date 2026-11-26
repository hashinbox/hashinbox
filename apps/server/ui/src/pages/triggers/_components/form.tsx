import Box from '@mui/material/Box';
import Button from '@mui/material/Button';
import FormControlLabel from '@mui/material/FormControlLabel';
import Stack from '@mui/material/Stack';
import Step from '@mui/material/Step';
import StepButton from '@mui/material/StepButton';
import StepContent from '@mui/material/StepContent';
import StepLabel from '@mui/material/StepLabel';
import Stepper from '@mui/material/Stepper';
import Switch from '@mui/material/Switch';
import TextField from '@mui/material/TextField';
import Typography from '@mui/material/Typography';
import { useState, type ReactNode } from 'react';
import type { Wallet } from '@hashinbox/client';
import type { Shell } from '@/layout';
import { assetName, networkChoices } from '@/money';
import { mono } from '@/theme';
import { NetworkSelect } from '@/_components/network-select';
import { ActionPart } from './action';
import { Row, Word } from './logic';
import { CheckWords, FilterWords, SelectorWords } from './parts';
import {
  SELF,
  describeCompare,
  describeGate,
  describeLogic,
  describeSelector,
  describeSend,
  validId,
  type Choices,
  type Compare,
  type Definition,
} from './forms';

interface Props {
  shell: Shell;
  wallets: Wallet[];
  /** Every tag in use, offered when choosing wallets. */
  tags: string[];
  value: Definition;
  onChange: (value: Definition) => void;
  /** Whether it is stored: its id then never changes. */
  kept: boolean;
}

interface Part {
  title: string;
  hint: string;
  /** One line on it, shown while it is closed. */
  summary: string;
  /** What is wrong with it, if anything. */
  error?: string | undefined;
  body: ReactNode;
}

const COMPARE: Compare = {
  of: { this: {} },
  aggregate: 'sum',
  operator: 'above',
  value: { value: '0' },
};

/**
 * Any trigger, step by step: basics, target, filter, check and action, each
 * a full form of what the server takes, summed up in a line when closed.
 */
export function TriggerForm({ shell, wallets, tags, value: def, onChange, kept }: Props) {
  const [active, setActive] = useState(kept ? -1 : 0);
  const assets = shell.listings
    .filter((l) => l.network.chain === def.chain && l.network.name === def.network)
    .map((l) => l.asset);
  const name = (asset: string) => assetName(shell.listings, def.chain, def.network, asset);
  const choices: Choices = {
    self: SELF.one,
    chain: def.chain,
    network: def.network,
    assets,
    name,
    wallets,
    tags,
  };
  const together = { ...choices, self: SELF.kept };
  const update = (patch: Partial<Definition>) => onChange({ ...def, ...patch });

  const send = def.action.input;

  const parts: Part[] = [
    {
      title: 'Basics',
      hint: kept
        ? 'Its network, and whether it runs.'
        : 'Its id, network, and whether it runs. Triggers created together share the network and are created all or none.',
      summary: `${def.id || 'no id'} · ${def.chain}/${def.network} · ${def.enabled ? 'enabled' : 'disabled'}`,
      // Not red before anything is typed; an empty id still cannot be created.
      error: kept || def.id === '' || validId(def.id) ? undefined : 'Id: 1 to 64 of a-z, 0-9 and -',
      body: (
        <Stack
          direction="row"
          spacing={2}
          useFlexGap
          sx={{ alignItems: 'center', flexWrap: 'wrap' }}
        >
          {!kept && (
            <TextField
              size="small"
              label="Id"
              placeholder="sweep-deposits"
              value={def.id}
              error={def.id !== '' && !validId(def.id)}
              onChange={(e) => update({ id: e.target.value.toLowerCase() })}
              slotProps={{ htmlInput: { spellCheck: false } }}
              title="1 to 64 of a-z, 0-9 and -; never changes."
              required
              sx={{ width: 240 }}
            />
          )}
          <NetworkSelect
            value={`${def.chain}/${def.network}`}
            networks={networkChoices(shell.listings)}
            onChange={(value) => {
              const [chain = '', network = ''] = value.split('/');
              update({ chain, network });
            }}
            sx={{ width: 240 }}
          />
          <FormControlLabel
            control={
              <Switch
                size="small"
                checked={def.enabled}
                onChange={(e) => update({ enabled: e.target.checked })}
              />
            }
            label={def.enabled ? 'Enabled' : 'Disabled'}
            sx={{ m: 0, gap: 1 }}
          />
        </Stack>
      ),
    },
    {
      title: 'Target',
      hint: 'The wallets this trigger watches. Tag wallets on the Wallets page.',
      summary: describeSelector(def.selector, name),
      body: (
        <Row>
          <Word>Wallets</Word>
          <SelectorWords
            value={def.selector}
            choices={choices}
            onChange={(selector) => update({ selector })}
          />
        </Row>
      ),
    },
    {
      title: 'Filter',
      hint: 'Runs for each wallet on its own: “this wallet” is the wallet being checked. Wallets that pass are kept; the others sit this round out.',
      summary: def.filter
        ? describeLogic(def.filter, (l) => describeCompare(l.compare, name, SELF.one))
        : 'none: every wallet is kept',
      body: def.filter ? (
        <Row onRemove={() => update({ filter: null })}>
          <Word>Keep a wallet when</Word>
          <FilterWords
            value={def.filter}
            choices={choices}
            onChange={(filter) => update({ filter })}
          />
        </Row>
      ) : (
        <Row>
          <Word>Every wallet is kept.</Word>
          <Button onClick={() => update({ filter: { compare: COMPARE } })}>Add a filter</Button>
        </Row>
      ),
    },
    {
      title: 'Check',
      hint: 'Runs once over all the kept wallets together: “the kept wallets” are all of them, so a total adds them up.',
      summary: describeLogic(def.check, (g) => describeGate(g, name, SELF.kept)),
      body: (
        <Row>
          <Word>Act when</Word>
          <CheckWords
            value={def.check}
            choices={together}
            onChange={(check) => update({ check })}
          />
        </Row>
      ),
    },
    {
      title: 'Action',
      hint: 'What it does once the check passes. One side may be many wallets: each kept wallet pays one recipient, or one sender pays each wallet. The amount is worked out per transfer, where “this wallet” is the one paying or being paid.',
      summary: describeSend(send, name),
      error:
        'this' in send.source && 'this' in send.destination
          ? 'From and to are both the kept wallets'
          : undefined,
      body: (
        <ActionPart
          action={def.action}
          choices={choices}
          onChange={(action) => update({ action })}
        />
      ),
    },
  ];

  return (
    <Stepper nonLinear orientation="vertical" activeStep={active}>
      {parts.map((part, i) => (
        <Step key={part.title} completed={false}>
          <StepButton
            onClick={() => setActive(active === i ? -1 : i)}
            optional={
              active !== i && (
                <Typography
                  noWrap
                  sx={{
                    fontFamily: mono,
                    fontSize: 11.5,
                    color: part.error ? 'error.main' : 'text.secondary',
                  }}
                >
                  {part.error ?? part.summary}
                </Typography>
              )
            }
            sx={{ minWidth: 0, '& .MuiStepLabel-labelContainer': { minWidth: 0 } }}
          >
            <StepLabel error={part.error !== undefined}>{part.title}</StepLabel>
          </StepButton>
          <StepContent>
            <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
              {part.hint}
            </Typography>
            <Box sx={{ display: 'grid', gap: 1.25 }}>{part.body}</Box>
            <Stack direction="row" spacing={1} sx={{ mt: 2.5, mb: 1 }}>
              {i > 0 && (
                <Button color="inherit" onClick={() => setActive(i - 1)}>
                  Back
                </Button>
              )}
              <Button
                variant="contained"
                onClick={() => setActive(i + 1 < parts.length ? i + 1 : -1)}
              >
                {i + 1 < parts.length ? 'Next' : 'Done'}
              </Button>
            </Stack>
          </StepContent>
        </Step>
      ))}
    </Stepper>
  );
}
