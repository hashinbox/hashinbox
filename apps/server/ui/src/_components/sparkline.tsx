import Box from '@mui/material/Box';
import { useTheme } from '@mui/material/styles';
import { useState, type PointerEvent } from 'react';

const WIDTH = 240;
const HEIGHT = 40;
const PAD = 3;

/**
 * One series as a small line, oldest left, the last point marked. Hovered, a
 * crosshair snaps to the nearest point and a readout names it: `labels[i]`
 * and `format(values[i])`.
 */
export function Sparkline({
  values,
  labels,
  format = String,
  label,
}: {
  values: number[];
  labels: string[];
  format?: (value: number) => string;
  /** What the series is, for screen readers. */
  label: string;
}) {
  const theme = useTheme();
  const [hover, setHover] = useState<number | null>(null);
  if (values.length < 2) return null;
  const top = Math.max(...values, 0);
  const step = (WIDTH - PAD * 2) / (values.length - 1);
  const x = (i: number) => PAD + i * step;
  const y = (v: number) => HEIGHT - PAD - (top === 0 ? 0 : (v / top) * (HEIGHT - PAD * 2));
  const points = values.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(' ');
  const last = values.length - 1;
  const move = (event: PointerEvent<SVGSVGElement>) => {
    const box = event.currentTarget.getBoundingClientRect();
    const at = ((event.clientX - box.left) / box.width) * WIDTH;
    setHover(Math.min(last, Math.max(0, Math.round((at - PAD) / step))));
  };
  const shown = hover ?? last;
  const ink = theme.palette.text.secondary;
  return (
    <Box sx={{ position: 'relative' }}>
      <svg
        viewBox={`0 0 ${String(WIDTH)} ${String(HEIGHT)}`}
        width="100%"
        height={HEIGHT}
        preserveAspectRatio="none"
        role="img"
        aria-label={label}
        onPointerMove={move}
        onPointerLeave={() => setHover(null)}
        style={{ display: 'block', overflow: 'visible', cursor: 'crosshair' }}
      >
        <line
          x1={PAD}
          x2={WIDTH - PAD}
          y1={HEIGHT - PAD}
          y2={HEIGHT - PAD}
          stroke={theme.palette.divider}
          vectorEffect="non-scaling-stroke"
        />
        <polyline
          points={points}
          fill="none"
          stroke={ink}
          strokeWidth={1.5}
          strokeLinejoin="round"
          strokeLinecap="round"
          vectorEffect="non-scaling-stroke"
        />
        {hover !== null && (
          <line
            x1={x(hover)}
            x2={x(hover)}
            y1={0}
            y2={HEIGHT}
            stroke={theme.palette.text.disabled}
            vectorEffect="non-scaling-stroke"
          />
        )}
      </svg>
      <Box
        aria-hidden
        sx={{
          position: 'absolute',
          left: `${String((x(shown) / WIDTH) * 100)}%`,
          top: y(values[shown] ?? 0) - 4,
          width: 8,
          height: 8,
          ml: '-4px',
          borderRadius: '50%',
          bgcolor: 'text.primary',
          border: 2,
          borderColor: 'background.paper',
          pointerEvents: 'none',
        }}
      />
      <Box
        sx={{
          mt: 0.5,
          fontSize: 11,
          color: 'text.secondary',
          fontVariantNumeric: 'tabular-nums',
          minHeight: 16,
        }}
      >
        {labels[shown]} · {format(values[shown] ?? 0)}
      </Box>
    </Box>
  );
}
