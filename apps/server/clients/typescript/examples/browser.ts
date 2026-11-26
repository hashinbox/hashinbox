// A page holding a token its backend issued (examples/backend.ts): it checks
// what the token allows, then reads the wallet. It never holds a secret.
import {
  HashinboxClient,
  Session,
  walletGet,
  walletGetOperation,
} from "../src/index";

export async function showWallet(token: string, id: string): Promise<string> {
  const session = Session.parse(token);
  if (session.expired()) return "Expired: reload the page.";
  if (!session.can(walletGetOperation, { id })) return "Not yours.";
  const api = new HashinboxClient("https://hashinbox.example", { session });
  const wallet = await walletGet(api, { id });
  return `${wallet.address} on ${wallet.chain}/${wallet.network}`;
}
