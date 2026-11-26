// A Node backend holding a key: it creates a customer's wallet, then hands
// the customer's browser a 15-minute token that reads only that wallet. The
// secret never leaves this process; the client signs its own sessions with
// it. The machine's clock must be roughly right (the server takes an `iat`
// at most 60 s ahead, and at most 24 h of life).
import { HashinboxClient, Secret, walletsCreate } from "../src/index";

const secret = Secret.parse(
  process.env.HASHINBOX_KEY_ID ?? "",
  process.env.HASHINBOX_KEY_SECRET ?? "",
);
const api = new HashinboxClient(process.env.HASHINBOX_URL, { secret });

/** A new deposit wallet for `customer`, and the token its page uses. */
export async function depositFor(customer: string) {
  const wallet = await walletsCreate(api, {
    body: { chain: "ethereum", network: "mainnet", external_id: customer },
  });
  const session = await secret.issue({
    scopes: `com.hashinbox.wallets[${wallet.id}].$read`,
    subject: customer,
    ttl: 15 * 60,
  });
  return {
    wallet: wallet.id,
    token: session.token,
    expiresAt: session.expiresAt,
  };
}
