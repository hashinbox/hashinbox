import Alert from '@mui/material/Alert';
import Stack from '@mui/material/Stack';
import { Choice, Row, Word } from './logic';
import { AmountWords, TargetWords } from './parts';
import { SELF, type Action, type Choices } from './forms';

/** A send, as sentences: what asset, from where, to where, and how much each time. */
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
  const kept = { ...choices, self: SELF.kept };
  const one = { ...choices, self: SELF.one };
  return (
    <Stack spacing={1}>
      <Row>
        <Word>Send</Word>
        <Choice
          label="Asset"
          value={send.asset}
          options={choices.assets.map((a) => ({ value: a, label: choices.name(a) }))}
          onChange={(asset) => set({ asset })}
        />
      </Row>
      <Row>
        <Word>from</Word>
        <TargetWords value={send.source} choices={kept} onChange={(source) => set({ source })} />
      </Row>
      <Row>
        <Word>to</Word>
        <TargetWords
          value={send.destination}
          choices={kept}
          onChange={(destination) => set({ destination })}
        />
      </Row>
      <Row>
        <Word>each transfer moving the</Word>
        <AmountWords value={send.amount} choices={one} onChange={(amount) => set({ amount })} />
      </Row>
      {'this' in send.source && 'this' in send.destination && (
        <Alert severity="warning" variant="outlined">
          From and to cannot both be the kept wallets; pick the wallets on one side.
        </Alert>
      )}
    </Stack>
  );
}
