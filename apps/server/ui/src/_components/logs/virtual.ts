// A list too long to render whole: only the items in view, and a buffer
// either side, are in the DOM; the rest is two spacers. An item's height is
// `guess` until it is rendered and measured.
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type RefObject } from 'react';

/** How far beyond the view items are rendered, in px. */
const BUFFER_PX = 600;

/** The scroller's offset and height. */
interface View {
  top: number;
  height: number;
}

/** The first item in view, and how far the view's top is into it. */
interface First {
  key: string;
  delta: number;
}

/** Each item's top, and the height of them all. */
function offsetsOf(keys: string[], heights: Map<string, number>, guess: number) {
  const tops: number[] = [];
  let total = 0;
  for (const key of keys) {
    tops.push(total);
    total += heights.get(key) ?? guess;
  }
  return { tops, total };
}

/** The index of the last item whose top is at or above `y`. */
function indexAt(tops: number[], y: number): number {
  let low = 0;
  let high = tops.length - 1;
  while (low < high) {
    const mid = Math.ceil((low + high) / 2);
    if ((tops[mid] ?? 0) <= y) low = mid;
    else high = mid - 1;
  }
  return Math.max(low, 0);
}

/**
 * The items of `keys` to render in `scroller`: `start` to `end`, after
 * `before` px and followed by `after` px. `measure` goes on each rendered
 * item's element (with `data-key`). When `keys` change and `anchored`,
 * the item that was first in view stays where it was.
 */
export function useVirtual(
  keys: string[],
  anchored: boolean,
  scroller: RefObject<HTMLElement | null>,
  guess: number,
) {
  const [heights, setHeights] = useState(() => new Map<string, number>());
  const [view, setView] = useState<View>({ top: 0, height: 0 });
  const observer = useRef<ResizeObserver | null>(null);
  const first = useRef<First | null>(null);
  const { tops, total } = offsetsOf(keys, heights, guess);

  const onResize = useCallback((seen: ResizeObserverEntry[]) => {
    setHeights((was) => {
      const next = new Map(was);
      for (const { target } of seen) {
        const key = (target as HTMLElement).dataset.key;
        if (key !== undefined) next.set(key, target.getBoundingClientRect().height);
      }
      return next;
    });
  }, []);

  const measure = useCallback(
    (element: HTMLElement | null) => {
      if (element === null) return;
      observer.current ??= new ResizeObserver(onResize);
      const watching = observer.current;
      watching.observe(element);
      return () => watching.unobserve(element);
    },
    [onResize],
  );

  const onScroll = () => {
    const element = scroller.current;
    if (element) setView({ top: element.scrollTop, height: element.clientHeight });
  };

  useEffect(() => {
    const element = scroller.current;
    if (!element) return;
    const sizing = new ResizeObserver(() =>
      setView({ top: element.scrollTop, height: element.clientHeight }),
    );
    sizing.observe(element);
    return () => {
      sizing.disconnect();
      observer.current?.disconnect();
    };
  }, [scroller]);

  // Keys changed: put the item first in view before back where it was.
  useLayoutEffect(() => {
    const element = scroller.current;
    const was = first.current;
    if (!anchored || !element || !was) return;
    const index = keys.indexOf(was.key);
    if (index >= 0) element.scrollTo({ top: (tops[index] ?? 0) + was.delta });
    // `tops` follows `keys`; only a change of keys moves the view.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keys]);

  // After every render: remember the item first in view, for the next change of keys.
  useLayoutEffect(() => {
    const element = scroller.current;
    if (!element || keys.length === 0) return;
    const index = indexAt(tops, element.scrollTop);
    const key = keys[index];
    if (key !== undefined) first.current = { key, delta: element.scrollTop - (tops[index] ?? 0) };
  });

  const start = indexAt(tops, view.top - BUFFER_PX);
  const end = Math.min(keys.length, indexAt(tops, view.top + view.height + BUFFER_PX) + 1);
  return {
    start,
    end,
    before: tops[start] ?? 0,
    after: total - (tops[end] ?? total),
    measure,
    onScroll,
  };
}
