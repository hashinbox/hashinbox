// Hand-written: a session JWT (design 096), decoded but not verified: only
// its issuer can check it, and the server does.

import { can, type Operation } from "./client";
import type { Amr } from "./models";
import { Scopes, type Packed } from "./scope";

/** A session: a JWT and the claims it carries. */
export class Session {
  /** The JWT, sent as `Authorization: Bearer …`. */
  readonly token: string;
  /** `iss` (= the header's `kid`): `user/{id}`, or a key's `hip/…`. */
  readonly issuer: string;
  /** `sub`: a user's id, or who a key's session is for. */
  readonly subject: string | undefined;
  /** What it may do, unpacked. */
  readonly scopes: Scopes;
  /** `iat`, Unix seconds. */
  readonly issuedAt: number;
  /** `exp`, Unix seconds. */
  readonly expiresAt: number;
  /** How its login proved who it is; empty for a key's. */
  readonly amr: readonly Amr[];

  private constructor(token: string, claims: Claims) {
    this.token = token;
    this.issuer = claims.iss;
    this.subject = claims.sub;
    this.scopes = Scopes.parse(claims.scopes);
    this.issuedAt = claims.iat;
    this.expiresAt = claims.exp;
    this.amr = claims.amr ?? [];
  }

  /** The session `token` is: its claims decoded, its signature not checked. */
  static parse(token: string): Session {
    const parts = token.split(".");
    if (parts.length !== 3) throw new TypeError("not a JWT");
    const header = decodePart(parts[0]!) as { alg?: string; kid?: string };
    const claims = decodePart(parts[1]!) as Claims;
    if (header.alg !== "HS256") throw new TypeError("its alg is not HS256");
    if (typeof claims.iss !== "string" || header.kid !== claims.iss)
      throw new TypeError("its header does not name its issuer");
    if (typeof claims.iat !== "number" || typeof claims.exp !== "number")
      throw new TypeError("it has no times");
    return new Session(token, claims);
  }

  /** Whether it has expired at `now` (Unix seconds; the clock's by default). */
  expired(now = Date.now() / 1000): boolean {
    return this.expiresAt <= now;
  }

  /** Whether it holds every scope `operation` needs for `params`. */
  can(operation: Operation, params: object = {}): boolean {
    return can(this.scopes, operation, params);
  }
}

/** A session JWT's claims. */
interface Claims {
  iss: string;
  sub?: string;
  iat: number;
  exp: number;
  scopes: Packed;
  amr?: Amr[];
}

/** A JWT part's JSON. */
function decodePart(part: string): unknown {
  const base64 = part.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(base64.padEnd(Math.ceil(base64.length / 4) * 4, "="));
  const bytes = Uint8Array.from(binary, (char) => char.charCodeAt(0));
  return JSON.parse(new TextDecoder().decode(bytes));
}
