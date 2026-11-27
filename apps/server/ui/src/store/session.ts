// This tab's session and the user it is. Only the token is kept (in
// sessionStorage, `hashinbox.session`); the session is parsed from it again
// on load. `api` (`@/api/client`) sends the session held here.
import { create } from 'zustand';
import { createJSONStorage, persist } from 'zustand/middleware';
import { Session, type Operation, type UserRead } from '@hashinbox/client';

interface SessionState {
  session: Session | undefined;
  /** The logged-in user (`GET /v1/user`), once read. */
  me: UserRead | null;
  /** The server refused the session sent: the layout says so. */
  ended: boolean;
  setSession: (session: Session | undefined) => void;
  setMe: (me: UserRead | null) => void;
  /** Forgets the session; `ended`: the server refused it. */
  logout: (ended?: boolean) => void;
}

/** The session of a kept token, if it parses. */
function restore(kept: unknown): Session | undefined {
  const token = (kept as { token?: unknown } | undefined)?.token;
  if (typeof token !== 'string') return undefined;
  try {
    return Session.parse(token);
  } catch {
    return undefined;
  }
}

export const useSession = create<SessionState>()(
  persist(
    (set) => ({
      session: undefined,
      me: null,
      ended: false,
      setSession: (session) => set(session ? { session, ended: false } : { session, me: null }),
      setMe: (me) => set({ me }),
      logout: (ended = false) => set({ session: undefined, me: null, ended }),
    }),
    {
      name: 'hashinbox.session',
      storage: createJSONStorage(() => sessionStorage),
      partialize: (state) => ({ token: state.session?.token }),
      merge: (kept, current) => ({ ...current, session: restore(kept) }),
    },
  ),
);

/** Whether the session holds every scope `operation` needs for `params`. */
export function useCan(operation: Operation, params: object = {}): boolean {
  return useSession((state) => state.session?.can(operation, params) === true);
}
