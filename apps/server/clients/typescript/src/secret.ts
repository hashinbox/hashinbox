// Hand-written: a key's secret (design 096) and the sessions it signs. The
// secret stays where it is: only the JWTs it signs are sent.

import { packEntries, Scopes, type Packed } from "./scope";
import { Session } from "./session";

/** The longest a session lasts: 24 hours, in seconds. */
export const MAX_TTL = 24 * 60 * 60;

/** What `Secret.issue` signs. */
export interface Issue {
  /** What it may do: covered by the key's, or the server refuses it. */
  scopes: Scopes | Packed | string;
  /** Who it is for (a customer): logged, grants nothing. */
  subject?: string;
  /** Seconds it lasts, at most `MAX_TTL`; 600 by default. */
  ttl?: number;
  /** `iat`, Unix seconds; the clock's by default. */
  issuedAt?: number;
}

/**
 * A key: its client id (`hip/…`) and its secret (`his/…`), the HS256 key of
 * the sessions it signs. The machine's clock must be roughly right: the
 * server accepts an `iat` at most 60 s ahead and refuses an expired `exp`.
 */
export class Secret {
  readonly id: string;
  readonly #value: string;

  private constructor(id: string, value: string) {
    this.id = id;
    this.#value = value;
  }

  /** The key `id` and `value` are; throws when `value` fails its checksum. */
  static parse(id: string, value: string): Secret {
    if (!/^hip\/.+/.test(id)) throw new TypeError("a key id is `hip/…`");
    const bytes = value.startsWith("his/")
      ? base58Decode(value.slice(4))
      : undefined;
    if (bytes?.length !== 36) throw new TypeError("not a `his/…` secret");
    const crc = crc32(bytes.subarray(0, 32));
    const kept = new DataView(bytes.buffer, bytes.byteOffset + 32, 4);
    if (kept.getUint32(0) !== crc)
      throw new TypeError("the secret's checksum does not match: a typo?");
    return new Secret(id, value);
  }

  /** A session of the key, HS256-signed here: `iss` = `kid` = its id. */
  async issue({
    scopes,
    subject,
    ttl = 600,
    issuedAt = Math.floor(Date.now() / 1000),
  }: Issue): Promise<Session> {
    if (!(ttl > 0 && ttl <= MAX_TTL))
      throw new RangeError(`ttl is 1 to ${MAX_TTL} seconds`);
    const held = scopes instanceof Scopes ? scopes : Scopes.parse(scopes);
    const header = `{"typ":"JWT","alg":"HS256","kid":${JSON.stringify(this.id)}}`;
    const packed = packEntries(held)
      .map(
        ([prefix, list]) => `${JSON.stringify(prefix)}:${JSON.stringify(list)}`,
      )
      .join(",");
    const claims =
      `{"iss":${JSON.stringify(this.id)}` +
      (subject === undefined ? "" : `,"sub":${JSON.stringify(subject)}`) +
      `,"iat":${issuedAt},"exp":${issuedAt + ttl},"scopes":{${packed}}}`;
    const body = `${base64url(utf8(header))}.${base64url(utf8(claims))}`;
    const key = await crypto.subtle.importKey(
      "raw",
      utf8(this.#value),
      { name: "HMAC", hash: "SHA-256" },
      false,
      ["sign"],
    );
    const signature = await crypto.subtle.sign("HMAC", key, utf8(body));
    return Session.parse(`${body}.${base64url(new Uint8Array(signature))}`);
  }
}

function utf8(text: string): Uint8Array<ArrayBuffer> {
  return new TextEncoder().encode(text);
}

function base64url(bytes: Uint8Array): string {
  const binary = String.fromCharCode(...bytes);
  return btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

const ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/** Base 58 (Bitcoin's alphabet) `text`'s bytes; `undefined` for a foreign character. */
function base58Decode(text: string): Uint8Array | undefined {
  let number = 0n;
  for (const char of text) {
    const digit = ALPHABET.indexOf(char);
    if (digit < 0) return undefined;
    number = number * 58n + BigInt(digit);
  }
  const bytes: number[] = [];
  for (; number > 0n; number >>= 8n) bytes.unshift(Number(number & 0xffn));
  const zeros = /^1*/.exec(text)![0].length;
  return new Uint8Array([...new Array<number>(zeros).fill(0), ...bytes]);
}

/** The CRC32 (ISO-HDLC, as zip and PNG) of `bytes`. */
function crc32(bytes: Uint8Array): number {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++)
      crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
  }
  return (crc ^ 0xffffffff) >>> 0;
}
