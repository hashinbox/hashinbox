// The dashboard's use: a login with its two-factor code when asked, the
// session kept in this tab (sessionStorage), and a lost session sending the
// user back to the login.
import {
  ApiError,
  HashinboxClient,
  Session,
  walletsList,
  walletsListOperation,
} from "../src/index";

const KEY = "hashinbox.session";

function stored(): Session | undefined {
  const token = sessionStorage.getItem(KEY);
  return token === null ? undefined : Session.parse(token);
}

export const api = new HashinboxClient("", {
  session: stored(),
  onSession: (session) => {
    if (session) sessionStorage.setItem(KEY, session.token);
    else sessionStorage.removeItem(KEY);
  },
  onUnauthorized: () => {
    api.session = undefined;
    location.assign("/login");
  },
});

/** Logs in; `"code"` when the user's two-factor code is needed, `"wrong"` for bad details. */
export async function login(
  email: string,
  password: string,
  totp?: string,
): Promise<"ok" | "code" | "wrong"> {
  try {
    await api.login({ email, password, totp });
    return "ok";
  } catch (error) {
    if (!(error instanceof ApiError)) throw error;
    if (error.reason === "totp_required") return "code";
    if (error.reason === "wrong_credentials") return "wrong";
    throw error;
  }
}

/** The wallet ids, when the session may list them. */
export async function walletIds(): Promise<string[]> {
  if (!api.session?.can(walletsListOperation)) return [];
  return (await walletsList(api)).wallets.map((wallet) => wallet.id);
}
