// Hand-written: a key's secret, and the sessions it signs offline (design
// 096): HS256, the whole `his/…` text the HMAC key.

package hashinbox

import (
	"bytes"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"hash/crc32"
	"strings"
	"time"
)

// MaxTTL is the longest a session lasts: the server refuses more.
const MaxTTL = 24 * time.Hour

// defaultTTL is a signed session's life when none is asked.
const defaultTTL = 10 * time.Minute

// now is the clock sessions are signed by; tests set it.
var now = time.Now

// Secret is a key: its public client id `hip/…` and its secret `his/…`.
// It never goes over the wire; it signs sessions.
type Secret struct {
	ID    string
	Value string
}

// ParseSecret is the key id and value are: id `hip/…`, value `his/…`
// whose base-58 number is 32 random bytes and their CRC32.
func ParseSecret(id, value string) (Secret, error) {
	if rest, ok := strings.CutPrefix(id, "hip/"); !ok || rest == "" {
		return Secret{}, errors.New("hashinbox: a key id is hip/…")
	}
	digits, ok := strings.CutPrefix(value, "his/")
	if !ok {
		return Secret{}, errors.New("hashinbox: a key secret is his/…")
	}
	raw, ok := base58Decode(digits)
	if !ok || len(raw) != 36 || crc32.ChecksumIEEE(raw[:32]) != binary.BigEndian.Uint32(raw[32:]) {
		return Secret{}, errors.New("hashinbox: not a key secret, or mistyped")
	}
	return Secret{ID: id, Value: value}, nil
}

// String is its id only: a secret is never printed.
func (s Secret) String() string {
	return s.ID
}

// IssueOption is a choice of a session Issue signs.
type IssueOption func(*issue)

type issue struct {
	scopes  Scopes
	subject string
	ttl     time.Duration
}

// WithScopes is what the session may do, covered by the key's; none when
// not given.
func WithScopes(scopes Scopes) IssueOption {
	return func(i *issue) { i.scopes = scopes }
}

// WithSubject is who the session is for (a customer); logged, grants
// nothing.
func WithSubject(subject string) IssueOption {
	return func(i *issue) { i.subject = subject }
}

// WithTTL is how long the session lasts, whole seconds, 1 s to MaxTTL; 10
// minutes when not given.
func WithTTL(ttl time.Duration) IssueOption {
	return func(i *issue) { i.ttl = ttl }
}

// Issue signs a session of the key, issued now: HS256, `kid` and `iss` its
// id, the HMAC key its whole `his/…` text. The server accepts it while its
// clock is at most 60 s behind `iat` and before `exp`.
func (s Secret) Issue(opts ...IssueOption) (Session, error) {
	i := issue{ttl: defaultTTL}
	for _, opt := range opts {
		opt(&i)
	}
	ttl := int64(i.ttl / time.Second)
	if ttl < 1 || i.ttl > MaxTTL {
		return Session{}, fmt.Errorf("hashinbox: a session lasts 1 s to %s, not %s", MaxTTL, i.ttl)
	}
	iat := now().Unix()
	c := claims{Iss: s.ID, Sub: i.subject, Iat: iat, Exp: iat + ttl, Scopes: i.scopes.Pack()}
	head, err := part(header{Typ: "JWT", Alg: "HS256", Kid: s.ID})
	if err != nil {
		return Session{}, err
	}
	body, err := part(c)
	if err != nil {
		return Session{}, err
	}
	mac := hmac.New(sha256.New, []byte(s.Value))
	mac.Write([]byte(head + "." + body))
	token := head + "." + body + "." + base64.RawURLEncoding.EncodeToString(mac.Sum(nil))
	return ParseSession(token)
}

// part is v as a JWT part: JSON as serde_json writes it (no HTML
// escaping), base64url without padding.
func part(v any) (string, error) {
	var out bytes.Buffer
	encoder := json.NewEncoder(&out)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(bytes.TrimSuffix(out.Bytes(), []byte("\n"))), nil
}

// base58Alphabet is Bitcoin's.
const base58Alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

// base58Decode is the bytes base-58 text is, each leading `1` a zero byte;
// false for a character not in the alphabet.
func base58Decode(text string) ([]byte, bool) {
	var little []byte
	for i := 0; i < len(text); i++ {
		carry := strings.IndexByte(base58Alphabet, text[i])
		if carry < 0 {
			return nil, false
		}
		for j := range little {
			carry += int(little[j]) * 58
			little[j] = byte(carry)
			carry >>= 8
		}
		for ; carry > 0; carry >>= 8 {
			little = append(little, byte(carry))
		}
	}
	zeros := 0
	for zeros < len(text) && text[zeros] == '1' {
		zeros++
	}
	out := make([]byte, zeros+len(little))
	for i, b := range little {
		out[len(out)-1-i] = b
	}
	return out, true
}
