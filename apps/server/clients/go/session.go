// Hand-written: a session, as the JWT the server or a key's secret signed
// (design 096).

package hashinbox

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"
)

// Session is a JWT and what it says: who issued it (`user/…` or a key's
// `hip/…`), for whom, what it may do (its claim expanded with Depends, as
// the server checks it) and when. Its signature is not checked: only the
// server can.
type Session struct {
	Token     string
	Issuer    string
	Subject   string
	Scopes    Scopes
	IssuedAt  time.Time
	ExpiresAt time.Time
	Amr       []Amr
}

// header is a session JWT's header, in the server's order.
type header struct {
	Typ string `json:"typ"`
	Alg string `json:"alg"`
	Kid string `json:"kid"`
}

// claims are a session JWT's claims, in the server's order; it refuses
// any other.
type claims struct {
	Iss    string              `json:"iss"`
	Sub    string              `json:"sub,omitempty"`
	Iat    int64               `json:"iat"`
	Exp    int64               `json:"exp"`
	Jti    string              `json:"jti,omitempty"`
	Scopes map[string][]string `json:"scopes"`
	Amr    []Amr               `json:"amr,omitempty"`
}

// ParseSession is the session token is, decoded; its signature is not
// checked.
func ParseSession(token string) (Session, error) {
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return Session{}, errors.New("hashinbox: a session is a JWT of three parts")
	}
	payload, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return Session{}, fmt.Errorf("hashinbox: session claims: %w", err)
	}
	var c claims
	if err := json.Unmarshal(payload, &c); err != nil {
		return Session{}, fmt.Errorf("hashinbox: session claims: %w", err)
	}
	scopes, err := ParseScopes(c.Scopes)
	if err != nil {
		return Session{}, fmt.Errorf("hashinbox: session scopes: %w", err)
	}
	return Session{
		Token:     token,
		Issuer:    c.Iss,
		Subject:   c.Sub,
		Scopes:    scopes.Expand(Depends),
		IssuedAt:  time.Unix(c.Iat, 0),
		ExpiresAt: time.Unix(c.Exp, 0),
		Amr:       c.Amr,
	}, nil
}

// Expired reports whether it has expired at now.
func (s Session) Expired(now time.Time) bool {
	return !now.Before(s.ExpiresAt)
}

// Can reports whether it holds every scope op needs with params.
func (s Session) Can(op Operation, params map[string]string) bool {
	_, ok := s.missing(op, params)
	return ok
}

// missing is the first scope op needs with params it does not hold; true
// when none.
func (s Session) missing(op Operation, params map[string]string) (string, bool) {
	for _, text := range op.Scopes(params) {
		scope, err := ParseScope(text)
		if err != nil || !s.Scopes.Covers(scope) {
			return text, false
		}
	}
	return "", true
}
