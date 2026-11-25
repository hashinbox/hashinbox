import Box from '@mui/material/Box';
import { useTheme } from '@mui/material/styles';
import { useEffect, useRef, useState, type PointerEvent } from 'react';

const HEIGHT = 140;
const LEFT = 64;
const RIGHT = 8;
const TOP = 8;
const BOTTOM = 20;
const TICKS = [0, 1, 2];

/** One value at one moment (Unix ms). */
export interface Point {
  at: number;
  value: number;
}

/** `at` (Unix ms) as a local `Mon 14:05`. */
function clock(at: number): string {
  return new Date(at).toLocaleString([], { weekday: 'short', hour: '2-digit', minute: '2-digit' });
}

/**
 * `series` over `from`..`to` (Unix ms) as a line, with time and value axes.
 * Hovered, a crosshair snaps to the nearest point of `series` and a readout
 * names its time and value.
 */
export function LineChart({
  series,
  from,
  to,
  format,
  label,
}: {
  series: Point[];
  from: number;
  to: number;
  format: (value: number) => string;
  /** What the series is, for screen readers. */
  label: string;
}) {
  const theme = useTheme();
  const [hover, setHover] = useState<number | null>(null);
  // Drawn at its own width in px, so text stays 11 px at any width.
  const box = useRef<HTMLDivElement>(null);
  const [WIDTH, setWidth] = useState(0);
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const seen = new ResizeObserver(([entry]) => setWidth(entry?.contentRect.width ?? 0));
    seen.observe(el);
    return () => seen.disconnect();
  }, []);
  const values = series.map((p) => p.value);
  if (values.length === 0) return null;
  const pad =
    (Math.max(...values) - Math.min(...values)) * 0.05 || Math.abs(values[0] ?? 0) * 0.01 || 1;
  const low = Math.min(...values) - pad;
  const high = Math.max(...values) + pad;
  const x = (at: number) => LEFT + ((at - from) / (to - from)) * (WIDTH - LEFT - RIGHT);
  const y = (v: number) => TOP + ((high - v) / (high - low)) * (HEIGHT - TOP - BOTTOM);
  const line = (points: Point[]) =>
    points.map((p) => `${x(p.at).toFixed(1)},${y(p.value).toFixed(1)}`).join(' ');
  const move = (event: PointerEvent<SVGSVGElement>) => {
    const box = event.currentTarget.getBoundingClientRect();
    const px = ((event.clientX - box.left) / box.width) * WIDTH;
    const at = from + ((px - LEFT) / (WIDTH - LEFT - RIGHT)) * (to - from);
    const near = (i: number) => Math.abs((series[i]?.at ?? 0) - at);
    setHover(
      series.length === 0
        ? null
        : series.reduce((best, _, i) => (near(i) < near(best) ? i : best), 0),
    );
  };
  const shown = hover === null ? undefined : series[hover];
  const ink = theme.palette.text.secondary;
  return (
    <Box ref={box} sx={{ position: 'relative' }}>
      <svg
        width={WIDTH}
        height={HEIGHT}
        viewBox={`0 0 ${String(WIDTH)} ${String(HEIGHT)}`}
        role="img"
        aria-label={label}
        onPointerMove={move}
        onPointerLeave={() => setHover(null)}
        style={{ display: 'block', cursor: 'crosshair' }}
      >
        {TICKS.map((i) => {
          const v = low + ((high - low) * i) / (TICKS.length - 1);
          return (
            <g key={`y${String(i)}`}>
              <line
                x1={LEFT}
                x2={WIDTH - RIGHT}
                y1={y(v)}
                y2={y(v)}
                stroke={theme.palette.divider}
              />
              <text
                x={LEFT - 6}
                y={y(v)}
                textAnchor="end"
                dominantBaseline="middle"
                fontSize={11}
                fill={ink}
              >
                {format(v)}
              </text>
            </g>
          );
        })}
        {TICKS.map((i) => {
          const at = from + ((to - from) * i) / (TICKS.length - 1);
          const anchor = i === 0 ? 'start' : i === TICKS.length - 1 ? 'end' : 'middle';
          return (
            <text
              key={`x${String(i)}`}
              x={x(at)}
              y={HEIGHT - 6}
              textAnchor={anchor}
              fontSize={11}
              fill={ink}
            >
              {clock(at)}
            </text>
          );
        })}
        <polyline
          points={line(series)}
          fill="none"
          stroke={theme.palette.text.primary}
          strokeWidth={1.5}
          strokeLinejoin="round"
          strokeLinecap="round"
        />
        {series.length > 0 && (
          <circle
            cx={x(series[series.length - 1]?.at ?? to)}
            cy={y(series[series.length - 1]?.value ?? 0)}
            r={2.5}
            fill={theme.palette.text.primary}
          />
        )}
        {shown && (
          <>
            <line
              x1={x(shown.at)}
              x2={x(shown.at)}
              y1={TOP}
              y2={HEIGHT - BOTTOM}
              stroke={theme.palette.text.disabled}
            />
            <circle
              cx={x(shown.at)}
              cy={y(shown.value)}
              r={3.5}
              fill={theme.palette.text.primary}
              stroke={theme.palette.background.paper}
              strokeWidth={2}
            />
          </>
        )}
      </svg>
      {shown && (
        <Box
          sx={{
            position: 'absolute',
            top: 0,
            left: `${String((x(shown.at) / WIDTH) * 100)}%`,
            transform:
              x(shown.at) > WIDTH / 2 ? 'translateX(calc(-100% - 8px))' : 'translateX(8px)',
            px: 1,
            py: 0.5,
            fontSize: 11,
            whiteSpace: 'nowrap',
            fontVariantNumeric: 'tabular-nums',
            bgcolor: 'background.paper',
            border: 1,
            borderColor: 'divider',
            borderRadius: 1,
            pointerEvents: 'none',
          }}
        >
          <Box sx={{ color: 'text.secondary' }}>{new Date(shown.at).toLocaleString()}</Box>
          <Box>{format(shown.value)}</Box>
        </Box>
      )}
    </Box>
  );
}
